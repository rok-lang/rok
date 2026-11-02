//! Hand-off copies: `rok::core::marker::transfer` copies
//! every collection a handed value reaches, once each. The memo maps an
//! original to its copy so a collection reached twice is copied once and
//! a cycle ends.
//!
//! A key is an object word and three plain words (a view's range; zero
//! for a collection's own object). The object word may point into an
//! object or outside the heap (a `str`'s bytes): it is only compared and
//! hashed, never read through.
//!
//! A table is `[current, len, cap, (key0, key1, key2, key3, copy)..]`, a
//! word-traced object ([`SCAN_WORDS_TYPE_ID`]); the memo is the first
//! table and its `current` names the one in use (itself until it grows).
//! An ambiguous word pins what it reaches, so the noted addresses stay
//! valid while the memo is alive. Stores go through the write barrier: a
//! memo that got old must keep a young table (and entry) alive.

#![allow(clippy::many_single_char_names)]
#![allow(clippy::cast_ptr_alignment)] // GC payloads are word-aligned

use core::mem::size_of;

use rokr_common::object_header::HEADER_SIZE;
use rokr_gc::SCAN_WORDS_TYPE_ID;

const W: usize = size_of::<usize>();

const CURRENT: usize = 0;
const LEN: usize = 1;
const CAP: usize = 2;
const ENTRIES: usize = 3;
/// Words per entry: four key words, then the copy.
const E: usize = 5;

const FIRST_CAP: usize = 16;

const fn words(obj: *mut u8) -> *mut usize {
    obj.wrapping_add(HEADER_SIZE).cast()
}

/// Store reference word `w` into `slot`, a word of `obj`, which may be
/// old: anything that could collect ran since `obj` was made.
///
/// # Safety
/// `obj` is a live GC object and `slot` a word of its payload.
pub unsafe fn store_ref(obj: *mut u8, slot: *mut usize, w: usize) {
    // SAFETY: caller contract.
    unsafe {
        rokr_gc::write_barrier(obj, slot.cast());
        *slot = w;
    }
}

/// Store `w` into `slot` of `obj`, made since anything last could
/// collect: young, so the write barrier has nothing to remember (and a
/// concurrent mark allocates it black, never reading it). Checked in a
/// debug runtime.
///
/// # Safety
/// As for [`store_ref`].
pub unsafe fn store_fresh(obj: *mut u8, slot: *mut usize, w: usize) {
    // SAFETY: caller contract.
    unsafe {
        debug_assert!(
            (*obj.cast::<ObjectHeader>())
                .gc_flags
                .load(Ordering::Relaxed)
                & FLAG_STICKY
                == 0,
            "rokr: an unbarriered store into an old object"
        );
        *slot = w;
    }
}

/// A zeroed table of `cap` entries (a power of two). May collect.
fn new_table(cap: usize) -> *mut u8 {
    let t = rokr_gc::alloc(HEADER_SIZE + W * (ENTRIES + E * cap), 8, SCAN_WORDS_TYPE_ID);
    // SAFETY: a fresh zeroed table with room for the counts.
    unsafe { *words(t).add(CAP) = cap };
    t
}

#[cfg(target_pointer_width = "64")]
const fn spread(k: [usize; 4]) -> usize {
    let mut h = k[0] >> 3;
    h = (h ^ k[1].wrapping_mul(0x9E37_79B9_7F4A_7C15)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h = (h ^ k[2].wrapping_mul(0x94D0_49BB_1331_11EB)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (h ^ k[3]).wrapping_mul(0xBF58_476D_1CE4_E5B9) >> 32
}

#[cfg(not(target_pointer_width = "64"))]
const fn spread(k: [usize; 4]) -> usize {
    let mut h = k[0] >> 3;
    h = (h ^ k[1].wrapping_mul(0x9E37_79B9)).wrapping_mul(0x85EB_CA6B);
    h = (h ^ k[2].wrapping_mul(0xC2B2_AE35)).wrapping_mul(0x9E37_79B9);
    (h ^ k[3]).wrapping_mul(0x85EB_CA6B) >> 8
}

/// Entry of key `k` in `t`: where it is, or the empty entry it would take.
///
/// # Safety
/// `t` is a live table that has an empty entry; `k[0]` is not 0.
unsafe fn probe(t: *mut u8, k: [usize; 4]) -> *mut usize {
    // SAFETY: caller contract; `i < cap`.
    unsafe {
        let w = words(t);
        let mask = *w.add(CAP) - 1;
        let mut i = spread(k) & mask;
        loop {
            let s = w.add(ENTRIES + E * i);
            if *s == 0
                || (*s == k[0] && *s.add(1) == k[1] && *s.add(2) == k[2] && *s.add(3) == k[3])
            {
                return s;
            }
            i = (i + 1) & mask;
        }
    }
}

/// A new, empty memo (`rok::core::marker::Memo`).
#[unsafe(no_mangle)]
pub extern "C" fn rok_xfer_memo() -> *mut u8 {
    rok_xfer_memo_for(0)
}

/// A new, empty memo with room for `n` entries before it grows.
#[unsafe(no_mangle)]
pub extern "C" fn rok_xfer_memo_for(n: usize) -> *mut u8 {
    let cap = n
        .saturating_mul(2)
        .saturating_add(2)
        .next_power_of_two()
        .max(FIRST_CAP);
    let m = new_table(cap);
    // SAFETY: fresh table.
    unsafe { store_fresh(m, words(m).add(CURRENT), m as usize) };
    m
}

// A visit (`marker::distinct`) asks whether two `par`
// items reach one collection. Each item reaches each object one of two
// ways: whole, or through slices. Slices only read, so items may share a
// buffer through them, as long as none reaches it whole; the elements
// they reach are visited one by one.
//
// The fast way stamps the object's header (`VISIT_*` bits of `gc_flags`)
// with the call's epoch: no table, no probe. `CUR` marks the objects of
// the item being visited, and is cleared after it. A stamp of a call
// still running elsewhere is never overwritten (`BUSY`); a stamp left by
// a call 65535 calls back reads as reached, so a clash found this way is
// confirmed the exact way: a memo of each object's owner.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use rokr_common::object_header::{
    FLAG_STICKY, ObjectHeader, VISIT_CUR, VISIT_OTHERS, VISIT_SLICED, VISIT_STAMP,
    VISIT_STAMP_SHIFT,
};

/// Reached for the first time in this item: visit what it holds.
const NEW: usize = 0;
/// Reached again, with more to visit (a whole reach after slices).
const AGAIN: usize = 1;
/// Reached by this item before: nothing more to do.
const MINE: usize = 2;
const CLASH: usize = 3;
/// Reached in this call, by this item or an earlier one: the caller
/// tells which (`HOW_MINE` / `HOW_OTHER`).
const SEEN: usize = 4;
/// Stamped by another call still running.
const BUSY: usize = 5;

const HOW_SLICE: u32 = 1;
const HOW_CUR: u32 = 2;
const HOW_MINE: u32 = 4;
const HOW_OTHER: u32 = 8;

const EPOCHS: usize = (VISIT_STAMP >> VISIT_STAMP_SHIFT) as usize + 1;
static NEXT_EPOCH: AtomicU32 = AtomicU32::new(1);
static ACTIVE: [AtomicU64; EPOCHS / 64] = [const { AtomicU64::new(0) }; EPOCHS / 64];

fn active(e: u32) -> bool {
    ACTIVE[e as usize / 64].load(Ordering::Acquire) & (1 << (e % 64)) != 0
}

/// A fresh epoch for one visit, never 0 nor one in use.
#[unsafe(no_mangle)]
pub extern "C" fn rok_visit_begin() -> u32 {
    loop {
        let e = NEXT_EPOCH.fetch_add(1, Ordering::Relaxed) % EPOCHS as u32;
        if e == 0 {
            continue;
        }
        let bit = 1 << (e % 64);
        if ACTIVE[e as usize / 64].fetch_or(bit, Ordering::AcqRel) & bit == 0 {
            return e;
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_visit_end(e: u32) {
    ACTIVE[e as usize / 64].fetch_and(!(1 << (e % 64)), Ordering::Release);
}

/// The next state of an object reached before in this call (`mine`:
/// by this item), for a reach `how`: what to do, and its new bits.
const fn step(f: u32, how: u32, mine: bool) -> (usize, u32) {
    let sliced = f & VISIT_SLICED != 0;
    if mine {
        if how & HOW_SLICE != 0 {
            return if sliced { (AGAIN, f) } else { (MINE, f) };
        }
        if !sliced {
            return (MINE, f);
        }
        if f & VISIT_OTHERS != 0 {
            return (CLASH, f);
        }
        return (AGAIN, f & !VISIT_SLICED);
    }
    if how & HOW_SLICE != 0 && sliced {
        // Slices of another item: reading only.
        let cur = if how & HOW_CUR != 0 { VISIT_CUR } else { 0 };
        return (NEW, f | VISIT_OTHERS | cur);
    }
    (CLASH, f)
}

/// The fast way: reach `obj` in epoch `e` (`how`: `HOW_*`). Returns
/// `NEW`, `AGAIN`, `MINE`, `CLASH`, `SEEN` or `BUSY`.
///
/// # Safety
/// `obj` is a live GC object.
unsafe fn mark(obj: *mut u8, e: u32, how: u32) -> usize {
    // SAFETY: caller contract.
    let flags = unsafe { &(*obj.cast::<ObjectHeader>()).gc_flags };
    let high = VISIT_STAMP | VISIT_OTHERS | VISIT_SLICED | VISIT_CUR;
    let mut f = flags.load(Ordering::Relaxed);
    loop {
        let stamp = (f & VISIT_STAMP) >> VISIT_STAMP_SHIFT;
        let (r, n) = if stamp != e {
            if stamp != 0 && active(stamp) {
                return BUSY;
            }
            let mut n = (f & !high) | (e << VISIT_STAMP_SHIFT);
            if how & HOW_SLICE != 0 {
                n |= VISIT_SLICED;
            }
            if how & HOW_CUR != 0 {
                n |= VISIT_CUR;
            }
            (NEW, n)
        } else if f & VISIT_CUR != 0 || how & HOW_MINE != 0 {
            step(f, how, true)
        } else if how & (HOW_CUR | HOW_OTHER) != 0 {
            step(f, how, false)
        } else {
            return SEEN;
        };
        if n == f {
            return r;
        }
        // The collector sets its own bits concurrently.
        match flags.compare_exchange_weak(f, n, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return r,
            Err(now) => f = now,
        }
    }
}

/// The exact way: reach `orig` for `owner` (from 1) in memo `m`, whose
/// entries hold `owner << 2 | OTHERS | SLICED`. Returns `NEW`, `AGAIN`,
/// `MINE` or `CLASH`. May collect.
///
/// # Safety
/// As for [`rok_xfer_note`].
unsafe fn exact_visit(m: *mut u8, orig: *mut u8, owner: usize, how: u32) -> usize {
    const SLICED: usize = 1;
    const OTHERS: usize = 2;
    // SAFETY: caller contract; a table always has an empty entry.
    unsafe {
        let t = *words(m).add(CURRENT) as *mut u8;
        let s = probe(t, [orig as usize, 0, 0, 0]);
        if *s == 0 {
            let sliced = if how & HOW_SLICE != 0 { SLICED } else { 0 };
            rok_xfer_note(m, orig, 0, 0, 0, ((owner << 2) | sliced) as *mut u8);
            return NEW;
        }
        let w = *s.add(4);
        // Owners and bits are small numbers: words the collector finds
        // ambiguous reach nothing through them.
        let mut f = 0;
        if w & SLICED != 0 {
            f |= VISIT_SLICED;
        }
        if w & OTHERS != 0 {
            f |= VISIT_OTHERS;
        }
        let (r, n) = step(f, how, w >> 2 == owner);
        let mut nw = owner << 2;
        if n & VISIT_SLICED != 0 {
            nw |= SLICED;
        }
        if n & VISIT_OTHERS != 0 {
            nw |= OTHERS;
        }
        *s.add(4) = nw;
        r
    }
}

// A visit's state is a word-traced object: the words below, `V_SEEN`
// naming a word table of the objects the item being visited reached (to
// tell its own apart, and clear their `CUR` bits). Ambiguous words pin:
// the noted addresses hold still.
const V_SEEN: usize = 0;
const V_MEMO: usize = 1;
const V_EPOCH: usize = 2;
const V_OWNER: usize = 3;
const V_CUR: usize = 4;
const V_CLASH: usize = 5;
const V_NSEEN: usize = 6;
const V_WORDS: usize = 7;

/// Up to this many objects an item's own are told apart by `seen`; past
/// it, by their `CUR` bit.
const FEW: usize = 8;

fn word_table(n: usize) -> *mut u8 {
    rokr_gc::alloc(HEADER_SIZE + W * n, 8, SCAN_WORDS_TYPE_ID)
}

/// Words a word table holds.
unsafe fn table_words(t: *mut u8) -> usize {
    // SAFETY: caller contract.
    unsafe { rokr_gc::payload_bytes(t) / W }
}

/// A visit's state, the header way in epoch `e`. May collect.
#[unsafe(no_mangle)]
pub extern "C" fn rok_visit_new(e: u32) -> *mut u8 {
    // SAFETY: no memo.
    unsafe { visit_new(e, core::ptr::null_mut()) }
}

/// A visit's state, the exact way with memo `memo`. May collect.
///
/// # Safety
/// `memo` is a memo from [`rok_xfer_memo_for`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_visit_exact(memo: *mut u8) -> *mut u8 {
    // SAFETY: caller contract.
    unsafe { visit_new(0, memo) }
}

unsafe fn visit_new(e: u32, memo: *mut u8) -> *mut u8 {
    // SAFETY: fresh tables; the memo stays reached from the state.
    unsafe {
        if !memo.is_null() {
            rokr_gc::pin(memo);
        }
        let v = word_table(V_WORDS);
        if !memo.is_null() {
            store_fresh(v, words(v).add(V_MEMO), memo as usize);
            rokr_gc::unpin(memo);
        }
        *words(v).add(V_EPOCH) = e as usize;
        *words(v).add(V_OWNER) = 1;
        rokr_gc::pin(v);
        let seen = word_table(2 * FEW);
        rokr_gc::unpin(v);
        // That allocation may have collected and made `v` old: without
        // the barrier a minor collection frees `seen` under the visit.
        store_ref(v, words(v).add(V_SEEN), seen as usize);
        v
    }
}

/// Append the object `o` to the table `v` names at word `at`, holding
/// `n` words: grown first when full. May collect.
unsafe fn append(v: *mut u8, at: usize, n: usize, o: *mut u8) {
    // SAFETY: caller contract; the state is pinned across the growth and
    // pins the table through its word. `o`, not noted yet, is pinned too:
    // moved, `seen` would keep its old address (a stale `CUR` bit).
    unsafe {
        let mut t = *words(v).add(at) as *mut u8;
        let cap = if t.is_null() { 0 } else { table_words(t) };
        if n == cap {
            rokr_gc::pin(v);
            rokr_gc::pin(o);
            let bigger = word_table((2 * cap).max(2 * FEW));
            rokr_gc::unpin(o);
            rokr_gc::unpin(v);
            for i in 0..n {
                store_fresh(bigger, words(bigger).add(i), *words(t).add(i));
            }
            store_ref(v, words(v).add(at), bigger as usize);
            t = bigger;
        }
        store_ref(t, words(t).add(n), o as usize);
    }
}

/// The item reaches `o` (`how`: `HOW_SLICE` or 0): `NEW` or `AGAIN` (its
/// elements are to be visited), `MINE`, or `CLASH` (noted). May collect.
///
/// # Safety
/// `v` is a visit state from [`rok_visit_new`]; `o` a live GC object.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_visit_reach(v: *mut u8, o: *mut u8, how: u32) -> usize {
    // SAFETY: caller contract.
    unsafe {
        let s = words(v);
        let memo = *s.add(V_MEMO) as *mut u8;
        if !memo.is_null() {
            let r = exact_visit(memo, o, *s.add(V_OWNER), how);
            if r == CLASH {
                *s.add(V_CLASH) = 1;
            }
            return r;
        }
        let e = *s.add(V_EPOCH) as u32;
        let cur = *s.add(V_CUR) != 0;
        let how = if cur { how | HOW_CUR } else { how };
        let mut r = mark(o, e, how);
        if r == SEEN {
            let seen = *s.add(V_SEEN) as *const usize;
            let seen = core::slice::from_raw_parts(seen.byte_add(HEADER_SIZE), *s.add(V_NSEEN));
            let mine = seen.contains(&(o as usize));
            r = mark(o, e, how | if mine { HOW_MINE } else { HOW_OTHER });
        }
        match r {
            NEW => {
                let n = *s.add(V_NSEEN);
                append(v, V_SEEN, n, o);
                let s = words(v);
                *s.add(V_NSEEN) = n + 1;
                if !cur && n + 1 > FEW {
                    *s.add(V_CUR) = 1;
                    let seen = words(*s.add(V_SEEN) as *mut u8);
                    for i in 0..=n {
                        let f = &(*(*seen.add(i) as *mut ObjectHeader)).gc_flags;
                        f.fetch_or(VISIT_CUR, Ordering::Relaxed);
                    }
                }
                NEW
            }
            CLASH | BUSY => {
                *s.add(V_CLASH) = 1;
                CLASH
            }
            r => r,
        }
    }
}

/// The item is done (the next one's objects are reached from now): its
/// objects lose their `CUR` bit. Whether it clashed with an earlier one.
///
/// # Safety
/// `v` is a visit state from [`rok_visit_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_visit_done(v: *mut u8) -> bool {
    // SAFETY: caller contract; the seen objects are pinned by the table.
    unsafe {
        let s = words(v);
        if *s.add(V_CUR) != 0 {
            let seen = words(*s.add(V_SEEN) as *mut u8);
            for i in 0..*s.add(V_NSEEN) {
                let f = &(*(*seen.add(i) as *mut ObjectHeader)).gc_flags;
                f.fetch_and(!VISIT_CUR, Ordering::Relaxed);
            }
            *s.add(V_CUR) = 0;
        }
        *s.add(V_NSEEN) = 0;
        *s.add(V_OWNER) += 1;
        *s.add(V_CLASH) != 0
    }
}

/// The copy noted in memo `m` for `orig` viewed as `(a, b, c)`, or
/// `orig` itself.
///
/// # Safety
/// `m` is a memo from [`rok_xfer_memo`]; `orig` is not 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_xfer_find(
    m: *mut u8,
    orig: *mut u8,
    a: usize,
    b: usize,
    c: usize,
) -> *mut u8 {
    // SAFETY: caller contract; a table always has an empty entry.
    unsafe {
        let t = *words(m).add(CURRENT) as *mut u8;
        let s = probe(t, [orig as usize, a, b, c]);
        if *s == 0 {
            return orig;
        }
        *s.add(4) as *mut u8
    }
}

/// Note `copy` as the copy of `orig` viewed as `(a, b, c)` in memo `m`.
/// May collect.
///
/// # Safety
/// `m` is a memo from [`rok_xfer_memo`]; the key is not noted yet and
/// `orig` is not 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_xfer_note(
    m: *mut u8,
    orig: *mut u8,
    a: usize,
    b: usize,
    c: usize,
    copy: *mut u8,
) {
    // SAFETY: caller contract. The table in use stays put while the
    // memo's word reaches it (ambiguous words pin).
    unsafe {
        let t = *words(m).add(CURRENT) as *mut u8;
        let (len, cap) = (*words(t).add(LEN), *words(t).add(CAP));
        // Into the table first (at most half full, so there is room):
        // the caller may not root `orig` past this call, and the growth
        // below may collect.
        let s = probe(t, [orig as usize, a, b, c]);
        store_ref(t, s, orig as usize);
        *s.add(1) = a;
        *s.add(2) = b;
        *s.add(3) = c;
        store_ref(t, s.add(4), copy as usize);
        *words(t).add(LEN) = len + 1;
        if 2 * (len + 1) <= cap {
            return;
        }
        // This call may be the memo's last use in every frame: pin it
        // (and through it the table) across the allocation.
        rokr_gc::pin(m);
        let bigger = new_table(2 * cap);
        rokr_gc::unpin(m);
        for i in 0..cap {
            let s = words(t).add(ENTRIES + E * i);
            if *s != 0 {
                let d = probe(bigger, [*s, *s.add(1), *s.add(2), *s.add(3)]);
                for j in 0..E {
                    store_fresh(bigger, d.add(j), *s.add(j));
                }
            }
        }
        *words(bigger).add(LEN) = len + 1;
        store_ref(m, words(m).add(CURRENT), bigger as usize);
    }
}

/// How many `size`-byte elements the GC object `buf` holds.
///
/// # Safety
/// `buf` is the start of a live GC object (never a view of a `str`'s
/// bytes).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_xfer_slots(buf: *mut u8, size: usize) -> usize {
    // SAFETY: caller contract.
    unsafe { rokr_gc::payload_bytes(buf) / size.max(1) }
}

/// Whether `buf` is the zero reference of a zeroed value.
#[unsafe(no_mangle)]
pub const extern "C" fn rok_xfer_null(buf: *mut u8) -> bool {
    buf.is_null()
}
