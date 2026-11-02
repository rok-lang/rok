//! A hand-off of a coroutine future
//! (`rok::core::future::hand_frame`): the new owner gets a frame of its
//! own, a word-for-word copy of the frame object that the copy's first
//! (priming) resume then fixes up.

#![allow(clippy::cast_ptr_alignment)] // GC payloads are word-aligned

use core::mem::size_of;
use core::ptr;
use core::sync::atomic::Ordering;

use rokr_common::object_header::{FLAG_STICKY, HEADER_SIZE, ObjectHeader};

const fn payload(obj: *mut u8) -> *mut usize {
    obj.wrapping_add(HEADER_SIZE).cast()
}

/// The env word of the `{code, env}` value in GC buffer `cell`.
///
/// # Safety
/// `cell` is a live GC buffer whose first element is a `fn` value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_fn_env(cell: *mut u8) -> *mut u8 {
    // SAFETY: caller contract.
    unsafe { *payload(cell).add(1) as *mut u8 }
}

/// Make `env` the env of the `fn` value in `cell`.
///
/// # Safety
/// As for [`rok_fn_env`]; `env` is a live GC object.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_fn_set_env(cell: *mut u8, env: *mut u8) {
    // SAFETY: caller contract.
    unsafe {
        crate::xfer::store_ref(cell, payload(cell).add(1), env as usize);
    }
}

/// The words of GC object `obj`'s payload (a frame's, for a copy).
///
/// # Safety
/// `obj` is a live GC object.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_obj_words(obj: *mut u8) -> usize {
    // SAFETY: caller contract.
    unsafe { rokr_gc::payload_bytes(obj).div_ceil(size_of::<usize>()) }
}

/// Make `dst`, a fresh word buffer of [`rok_obj_words`] of `src` with no
/// collection since, a copy of `src`: its type and words. Not a
/// safepoint.
///
/// # Safety
/// `src` is a live GC object; `dst` a fresh plain word buffer of its
/// size.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_obj_copy(dst: *mut u8, src: *mut u8) {
    // SAFETY: caller contract. Into a fresh object (`store_fresh`):
    // nothing to remember, and what it now names the original names too.
    // Plain until now, so no collection read it as another type.
    unsafe {
        debug_assert_eq!(rokr_gc::payload_bytes(dst), rokr_gc::payload_bytes(src));
        (*dst.cast::<ObjectHeader>()).type_id = (*src.cast::<ObjectHeader>()).type_id;
        debug_assert!(
            (*dst.cast::<ObjectHeader>())
                .gc_flags
                .load(Ordering::Relaxed)
                & FLAG_STICKY
                == 0,
            "rokr: a frame copy born old"
        );
        let bytes = rokr_gc::payload_bytes(src);
        ptr::copy_nonoverlapping(payload(src).cast::<u8>(), payload(dst).cast::<u8>(), bytes);
    }
}

/// Whether coroutine frame `frame` has not started (state 0) and holds
/// no reference: a hand-off copies its words and nothing else.
///
/// # Safety
/// `frame` is a live coroutine frame (`(i32 state, locals..)`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_frame_plain(frame: *mut u8) -> bool {
    // SAFETY: caller contract; the state is the payload's first word.
    unsafe { *payload(frame).cast::<i32>() == 0 && rokr_gc::refs_all_null(frame) }
}
