//! Lock-free Chase-Lev work-stealing deque of GC
//! references, for `rok::core::task`'s workers. The C11 formulation of
//! Lê, Pop, Cohen, Zappa Nardelli (`PPoPP` 2013); `t` / `b` / `r` name
//! top, bottom and ring as in the paper.
//!
//! The deque `[top, bottom, ring]` and ring `[mask, slots..]` are
//! word-traced GC objects ([`SCAN_WORDS_TYPE_ID`]), so queued items stay
//! alive through the deque, and a ring replaced by growth is garbage once
//! no thread is inside an operation, which holds at any collection since
//! these functions have no safepoint. Slots below `top` keep stale
//! references until overwritten; the ring never shrinks.

#![allow(clippy::many_single_char_names)]
#![allow(clippy::cast_ptr_alignment)] // GC payloads are word-aligned

use core::mem::size_of;
use core::sync::atomic::{AtomicIsize, AtomicUsize, Ordering, fence};

use rokr_common::object_header::HEADER_SIZE;
use rokr_gc::SCAN_WORDS_TYPE_ID;

const INITIAL: usize = 64;

#[repr(C)]
struct Deque {
    top: AtomicIsize,
    bottom: AtomicIsize,
    ring: AtomicUsize,
}

#[repr(C)]
struct Ring {
    mask: usize,
    // `slots: [AtomicUsize; mask + 1]` follows.
}

fn new_ring(cap: usize) -> *mut Ring {
    let obj = rokr_gc::alloc(
        HEADER_SIZE + size_of::<usize>() * (cap + 1),
        8,
        SCAN_WORDS_TYPE_ID,
    );
    // SAFETY: a fresh zeroed object with room for the mask and slots.
    unsafe {
        let r = obj.add(HEADER_SIZE).cast::<Ring>();
        (*r).mask = cap - 1;
        r
    }
}

/// # Safety
/// `r` is a live ring; `i` any index (wrapped by the mask).
unsafe fn slot<'a>(r: *mut Ring, i: isize) -> &'a AtomicUsize {
    // SAFETY: caller contract; `i & mask` is in bounds.
    unsafe { &*r.add(1).cast::<AtomicUsize>().add(i as usize & (*r).mask) }
}

/// # Safety
/// `d` is a deque handle from [`rok_deque_new`].
unsafe fn deque<'a>(d: *mut u8) -> &'a Deque {
    // SAFETY: caller contract.
    unsafe { &*d.add(HEADER_SIZE).cast::<Deque>() }
}

/// A new, empty deque (a GC object).
#[unsafe(no_mangle)]
pub extern "C" fn rok_deque_new() -> *mut u8 {
    let ring = new_ring(INITIAL);
    let ring_obj = ring.cast::<u8>().wrapping_sub(HEADER_SIZE);
    // The deque's allocation may collect: pin the (unrooted) ring
    // meanwhile.
    // SAFETY: `ring_obj` is a live GC object.
    unsafe { crate::rok_gc_pin(ring_obj) };
    let obj = rokr_gc::alloc(HEADER_SIZE + size_of::<Deque>(), 8, SCAN_WORDS_TYPE_ID);
    // SAFETY: fresh object; the ring is live.
    unsafe {
        deque(obj).ring.store(ring as usize, Ordering::Relaxed);
        crate::rok_gc_unpin(ring_obj);
    }
    obj
}

/// Owner only: push `item` (a GC reference) at the bottom.
///
/// # Safety
/// `d` is a deque handle; only one thread (its owner) pushes / pops.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_deque_push(d: *mut u8, item: usize) {
    // SAFETY: caller contract.
    let q = unsafe { deque(d) };
    let b = q.bottom.load(Ordering::Relaxed);
    let t = q.top.load(Ordering::Acquire);
    let mut r = q.ring.load(Ordering::Relaxed) as *mut Ring;
    // SAFETY: the ring is live while `d` is (the caller roots `d`).
    unsafe {
        if b - t > (*r).mask as isize {
            // Full: double. `item` is only on the caller's frame, which
            // roots it across the allocation.
            let bigger = new_ring(((*r).mask + 1) * 2);
            for i in t..b {
                slot(bigger, i).store(slot(r, i).load(Ordering::Relaxed), Ordering::Relaxed);
            }
            rokr_gc::write_barrier(d, (&raw const q.ring).cast());
            q.ring.store(bigger as usize, Ordering::Release);
            r = bigger;
        }
        rokr_gc::write_barrier(r.cast::<u8>().sub(HEADER_SIZE), slot(r, b).as_ptr().cast());
        slot(r, b).store(item, Ordering::Relaxed);
    }
    fence(Ordering::Release);
    q.bottom.store(b + 1, Ordering::Relaxed);
}

/// Owner only: pop the newest item into `out[index]` (a `GcBuf` of
/// references); 1 if there was one.
///
/// # Safety
/// As [`rok_deque_push`]; `out` is a GC buffer with room for `index`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_deque_pop(d: *mut u8, out: *mut u8, index: usize) -> i32 {
    // SAFETY: caller contract.
    let q = unsafe { deque(d) };
    let b = q.bottom.load(Ordering::Relaxed) - 1;
    let r = q.ring.load(Ordering::Relaxed) as *mut Ring;
    q.bottom.store(b, Ordering::Relaxed);
    fence(Ordering::SeqCst);
    let t = q.top.load(Ordering::Relaxed);
    if t > b {
        q.bottom.store(b + 1, Ordering::Relaxed);
        return 0;
    }
    // SAFETY: `t <= b`: slot `b` holds an item.
    let item = unsafe { slot(r, b) }.load(Ordering::Relaxed);
    if t == b {
        // The last item: race the thieves for it.
        let won = q
            .top
            .compare_exchange(t, t + 1, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok();
        q.bottom.store(b + 1, Ordering::Relaxed);
        if !won {
            return 0;
        }
    }
    // SAFETY: caller contract.
    unsafe {
        rokr_gc::write_barrier(out, out.add(HEADER_SIZE).cast::<usize>().add(index).cast());
        out.add(HEADER_SIZE).cast::<usize>().add(index).write(item);
    }
    1
}

/// Any thread (the owner too, for FIFO order): take the oldest item into
/// `out[index]`; 1 if there was one. Retries a lost race while items
/// remain.
///
/// # Safety
/// `d` is a deque handle; `out` is a GC buffer with room for `index`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_deque_steal(d: *mut u8, out: *mut u8, index: usize) -> i32 {
    // SAFETY: caller contract.
    let q = unsafe { deque(d) };
    loop {
        let t = q.top.load(Ordering::Acquire);
        fence(Ordering::SeqCst);
        let b = q.bottom.load(Ordering::Acquire);
        if t >= b {
            return 0;
        }
        let r = q.ring.load(Ordering::Acquire) as *mut Ring;
        // SAFETY: `t < b`; a ring stays live during the operation.
        let item = unsafe { slot(r, t) }.load(Ordering::Relaxed);
        if q.top
            .compare_exchange(t, t + 1, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            // SAFETY: caller contract.
            unsafe {
                rokr_gc::write_barrier(out, out.add(HEADER_SIZE).cast::<usize>().add(index).cast());
                out.add(HEADER_SIZE).cast::<usize>().add(index).write(item);
            }
            return 1;
        }
    }
}

/// Items in the deque (a racy snapshot).
///
/// # Safety
/// `d` is a deque handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_deque_len(d: *mut u8) -> usize {
    // SAFETY: caller contract.
    let q = unsafe { deque(d) };
    let b = q.bottom.load(Ordering::Relaxed);
    let t = q.top.load(Ordering::Relaxed);
    (b - t).max(0) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize as Counter;

    /// One owner pushing / popping, three thieves stealing: every item
    /// is taken exactly once, across ring growth.
    #[test]
    fn every_item_taken_once() {
        const N: usize = 20000;
        struct Ptr(*mut u8);
        // SAFETY: the deque is shared on purpose (it is a concurrent
        // structure); the test keeps it alive until all threads join.
        unsafe impl Send for Ptr {}
        unsafe impl Sync for Ptr {}
        let d = Ptr(rok_deque_new());
        let seen: Vec<Counter> = (0..N).map(|_| Counter::new(0)).collect();
        let done = core::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            for k in 0..3 {
                let (d, seen, done) = (&d, &seen, &done);
                s.spawn(move || {
                    // A zero (young) header, then the slots.
                    let mut out = [0usize; 2 + 4];
                    let buf = out.as_mut_ptr().cast::<u8>();
                    loop {
                        // SAFETY: `buf + HEADER_SIZE` is the slots.
                        if unsafe { rok_deque_steal(d.0, buf, k) } == 1 {
                            seen[out[2 + k] - 1].fetch_add(1, Ordering::Relaxed);
                        } else if done.load(Ordering::Acquire) {
                            break;
                        }
                    }
                });
            }
            let mut out = [0usize; 2 + 1];
            let buf = out.as_mut_ptr().cast::<u8>();
            for i in 0..N {
                // SAFETY: owner thread.
                unsafe { rok_deque_push(d.0, i + 1) };
                if i % 3 == 0 && unsafe { rok_deque_pop(d.0, buf, 0) } == 1 {
                    seen[out[2] - 1].fetch_add(1, Ordering::Relaxed);
                }
            }
            while unsafe { rok_deque_pop(d.0, buf, 0) } == 1 {
                seen[out[2] - 1].fetch_add(1, Ordering::Relaxed);
            }
            done.store(true, Ordering::Release);
        });
        assert!(seen.iter().all(|c| c.load(Ordering::Relaxed) == 1));
        // SAFETY: no thread uses the deque any more.
        assert_eq!(unsafe { rok_deque_len(d.0) }, 0);
    }
}
