//! 16-byte object header carried by every GC-managed allocation
//! (spec 008-abi/801, 007-runtime/702 §7.2.4).
//!
//! Layout (offsets in bytes):
//!   0  : `type_id`   u32
//!   4  : `gc_flags`  u32 (bit 0=MARK, 1=STICKY, 2=PINNED, 3=FORWARDED,
//!                          4=LARGE, 5=IMMORTAL (unused); 6=UNLOGGED, 7=CARDED
//!                          are this runtime's write-barrier bits,
//!                          8=NOMOVE, 9=NOEVAC its evacuation bits,
//!                          12=WIPE: zeroed when freed; 13..=31 a `par`
//!                          visit's, `VISIT_*`)
//!   8  : `gc_word`   u64 (the object's total size; the forwarding pointer
//!                          once evacuated)
//!   16 : payload     ...

use core::mem::{align_of, offset_of, size_of};
use core::sync::atomic::AtomicU32;

/// `gc_flags` is atomic because MARK / FORWARDED are set during
/// concurrent mark / evacuate (801 §8.1.3).
#[repr(C, align(8))]
pub struct ObjectHeader {
    pub type_id: u32,
    pub gc_flags: AtomicU32,
    pub gc_word: u64,
}

pub const FLAG_MARK: u32 = 1 << 0;
pub const FLAG_STICKY: u32 = 1 << 1;
pub const FLAG_PINNED: u32 = 1 << 2;
pub const FLAG_FORWARDED: u32 = 1 << 3;
pub const FLAG_LARGE: u32 = 1 << 4;
/// An old (`STICKY`) object not yet in the remembered set.
///
/// Implementation bit (801 §8.1.3 bits 6..). The write barrier's fast path tests
/// only this bit; the slow path (`rok_gc_wb_slow`) clears it and logs
/// the object.
pub const FLAG_UNLOGGED: u32 = crate::gc_abi::FLAG_UNLOGGED;
/// An old large object whose reference stores mark cards.
///
/// The word before the header points at its card table, one byte per
/// `rokr_gc::CARD_SIZE` bytes of the object (from the header).
/// Implementation bit; the barrier tests it together with `UNLOGGED`.
pub const FLAG_CARDED: u32 = crate::gc_abi::FLAG_CARDED;
/// The object's address was taken as a number (a parking key, 805
/// `rok_task_key_*`): the collector never moves it. Implementation bit;
/// permanent, unlike `PINNED` it does not keep the object alive.
pub const FLAG_NOMOVE: u32 = 1 << 8;
/// Collector-internal, one full collection: an ambiguous word (a root,
/// a runtime word buffer's word) refers to the object, so this
/// evacuation leaves it in place.
pub const FLAG_NOEVAC: u32 = 1 << 9;
/// Collector-internal, one concurrent mark: a store into this old object
/// is logging (or logged) its references as of the mark's start (SATB).
pub const FLAG_SATB: u32 = 1 << 10;
/// Collector-internal, one concurrent mark: the marker has scanned the
/// object, so the first store into it need not log its references.
pub const FLAG_SCANNED: u32 = 1 << 11;
/// A secret buffer (`rok_gc_secret_new`, a LOS object): the sweep that
/// frees it zeroes its payload first. Implementation bit; permanent.
pub const FLAG_WIPE: u32 = 1 << 12;

/// A `par` visit's (rokr `xfer`, stdlib 2.19.1): the epoch of the last
/// visit that reached the object. The collector keeps these bits.
pub const VISIT_STAMP_SHIFT: u32 = 13;
pub const VISIT_STAMP: u32 = 0xffff << VISIT_STAMP_SHIFT;
/// Reached by more than one item, each through slices.
pub const VISIT_OTHERS: u32 = 1 << 29;
/// Reached only through slices so far.
pub const VISIT_SLICED: u32 = 1 << 30;
/// Reached by the item being visited.
pub const VISIT_CUR: u32 = 1 << 31;

pub const HEADER_SIZE: usize = crate::gc_abi::HEADER_SIZE as usize;
pub const HEADER_ALIGN: usize = 8;

const _: () = {
    assert!(size_of::<ObjectHeader>() == HEADER_SIZE);
    assert!(align_of::<ObjectHeader>() == HEADER_ALIGN);
    assert!(offset_of!(ObjectHeader, type_id) == 0);
    assert!(offset_of!(ObjectHeader, gc_flags) == crate::gc_abi::GC_FLAGS_OFFSET as usize);
    assert!(offset_of!(ObjectHeader, gc_word) == crate::gc_abi::GC_WORD_OFFSET as usize);
};

impl ObjectHeader {
    /// Write a fresh header (702 §7.2.4): `type_id`, `gc_flags = 0`,
    /// `gc_word = 0`; the allocator then stores the size in `gc_word`.
    ///
    /// # Safety
    /// `obj` must be valid for writes of `HEADER_SIZE` bytes and
    /// `HEADER_ALIGN`-aligned.
    #[allow(clippy::cast_ptr_alignment)] // alignment is the caller contract
    pub const unsafe fn init(obj: *mut u8, type_id: u32) {
        let header = Self {
            type_id,
            gc_flags: AtomicU32::new(0),
            gc_word: 0,
        };
        // SAFETY: caller guarantees size and alignment.
        unsafe { obj.cast::<Self>().write(header) };
    }
}
