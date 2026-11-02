//! Executor lanes: blocking calls hand their worker to a new thread.
//!
//! Each pool thread has one malloc'd lane (`rok_tls_task_lane`). While
//! it is armed ([`rok_lane_arm`], i.e. polling a task), every blocking
//! runtime call ([`block_begin`] / [`block_end`]) bumps its tick and
//! marks it blocked. The monitor (`rok::core::task`) takes a lane blocked
//! across a whole tick ([`rok_lane_take`]): its worker moves to a new
//! thread, and the blocked thread finishes its poll as an orphan guest
//! and frees the lane. Only the OS-neutral word protocol lives here.

use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};

use core::ptr;

use rokr_common::tls;
use rokr_gc::world;

const RUNNING: u64 = 0;
const BLOCKED: u64 = 1;
const LOST: u64 = 2;

/// One pool thread's lane: `word` is `tick << 2 | state`.
struct Lane {
    word: AtomicU64,
    /// Polling a task (written by the owner only).
    armed: AtomicBool,
    /// The `GcBuf<Worker>` cell the lost thread installs (pinned).
    heir: AtomicUsize,
}

/// Lanes blocked right now.
static BLOCKED_LANES: AtomicUsize = AtomicUsize::new(0);
/// 1 while the monitor sleeps until some lane blocks.
static MONITOR: AtomicI32 = AtomicI32::new(0);

/// How long the monitor sleeps between scans: a call blocked for one to
/// two ticks loses its worker.
const TICK_NS: i64 = 1_000_000;

fn current() -> *mut Lane {
    // SAFETY: this thread's own slot.
    unsafe { tls::task_lane().read() as *mut Lane }
}

/// This thread's lane if it is armed.
fn armed() -> Option<&'static Lane> {
    // SAFETY: our own live lane (only we free it).
    let l = unsafe { current().as_ref() }?;
    l.armed.load(Ordering::Relaxed).then_some(l)
}

/// Before a blocking call: parked for the GC; a lane is marked blocked.
pub fn block_begin() {
    world::enter_native();
    let Some(lane) = armed() else { return };
    let w = lane.word.load(Ordering::Relaxed);
    lane.word
        .store((((w >> 2) + 1) << 2) | BLOCKED, Ordering::SeqCst);
    BLOCKED_LANES.fetch_add(1, Ordering::SeqCst);
    if MONITOR.load(Ordering::SeqCst) == 1 && MONITOR.swap(0, Ordering::SeqCst) == 1 {
        crate::thread::wake(&MONITOR);
    }
}

/// Back from a blocking call: running again; if the lane was taken
/// meanwhile, this thread goes on as the heir the monitor left.
pub fn block_end() {
    world::leave_native();
    let Some(lane) = armed() else { return };
    BLOCKED_LANES.fetch_sub(1, Ordering::SeqCst);
    let w = lane.word.load(Ordering::SeqCst);
    if w & 3 == BLOCKED
        && lane
            .word
            .compare_exchange(w, w & !3, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    {
        return;
    }
    // Taken: the monitor stored the heir before marking it lost.
    let heir = lane.heir.load(Ordering::SeqCst);
    crate::thread::rok_task_worker_set(heir);
    // SAFETY: our slot; the monitor no longer reads this lane (its worker
    // has a new one), so it is ours to free.
    unsafe {
        tls::task_lane().write(0);
        libc::free(ptr::from_ref(lane).cast_mut().cast());
    }
}

/// A new lane (running, tick 0).
#[unsafe(no_mangle)]
pub extern "C" fn rok_lane_new() -> usize {
    // SAFETY: plain allocation, initialized before use.
    unsafe {
        let l = libc::malloc(size_of::<Lane>()).cast::<Lane>();
        if l.is_null() {
            rokr_common::oom();
        }
        l.write(Lane {
            word: AtomicU64::new(RUNNING),
            armed: AtomicBool::new(false),
            heir: AtomicUsize::new(0),
        });
        l as usize
    }
}

/// This thread runs lane `l` (from [`rok_lane_new`]).
#[unsafe(no_mangle)]
pub extern "C" fn rok_lane_attach(l: usize) {
    // SAFETY: this thread's own slot.
    unsafe { tls::task_lane().write(l) };
}

/// Arm (`on != 0`) or disarm this thread's lane; the previous setting
/// (0 without a lane).
#[unsafe(no_mangle)]
pub extern "C" fn rok_lane_arm(on: i32) -> i32 {
    // SAFETY: our own live lane.
    let Some(l) = (unsafe { current().as_ref() }) else {
        return 0;
    };
    i32::from(l.armed.swap(on != 0, Ordering::Relaxed))
}

/// Lane `l`'s tick while it is blocked, else -1.
#[unsafe(no_mangle)]
pub extern "C" fn rok_lane_blocked(l: usize) -> i64 {
    // SAFETY: a live lane (the monitor only reads its workers' lanes).
    let w = unsafe { &*(l as *const Lane) }.word.load(Ordering::SeqCst);
    if w & 3 == BLOCKED {
        (w >> 2) as i64
    } else {
        -1
    }
}

/// Take lane `l` if it is still blocked in the call of `tick`: its thread
/// installs `heir` (a `GcBuf<Worker>` cell) when the call returns.
/// 1 if taken.
#[unsafe(no_mangle)]
pub extern "C" fn rok_lane_take(l: usize, tick: i64, heir: *mut u8) -> i32 {
    // SAFETY: a live lane; `heir` a live GC object (pinned while the
    // lane holds it).
    unsafe {
        let lane = &*(l as *const Lane);
        rokr_gc::pin(heir);
        lane.heir.store(heir as usize, Ordering::SeqCst);
        let from = ((tick as u64) << 2) | BLOCKED;
        let to = ((tick as u64) << 2) | LOST;
        if lane
            .word
            .compare_exchange(from, to, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return 1;
        }
        lane.heir.store(0, Ordering::SeqCst);
        rokr_gc::unpin(heir);
        0
    }
}

/// The monitor's pause between scans: one tick, or (`deep`) until some
/// lane blocks.
#[unsafe(no_mangle)]
pub extern "C" fn rok_lane_pause(deep: i32) {
    if deep != 0 {
        MONITOR.store(1, Ordering::SeqCst);
        // A lane that blocked before the store is counted here; one after
        // it sees the 1 and wakes us.
        if BLOCKED_LANES.load(Ordering::SeqCst) == 0 {
            crate::thread::wait_on(&MONITOR, 1, -1);
        }
        MONITOR.store(0, Ordering::SeqCst);
        return;
    }
    crate::thread::rok_thread_sleep_ns(TICK_NS);
}
