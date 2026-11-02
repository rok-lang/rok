//! Thread registry and stop-the-world.
//!
//! One process-wide "world lock" guards the heap, the registry and the
//! stop protocol. A collector holds it, sets `STOP`, raises every other
//! mutator's `rok_tls_yield_flag` and waits on the world condvar until
//! each one is out of `RUNNING`:
//!
//! - `GC_COOPERATING`: parked in [`safepoint`] (compiled code polls at
//!   entries and back edges) or blocked on the world lock
//!   ([`lock`] cooperates first);
//! - `PARKED`: in a blocking call bracketed by [`enter_native`] /
//!   [`leave_native`]; leaving waits out a running stop;
//! - not attached yet.
//!
//! Then every registered thread's shadow-stack chain is a root set.
//! TLABs keep ordinary allocation off the lock.

use core::cell::UnsafeCell;
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use rokr_common::sys;
use rokr_common::tls::{self, MUTATOR_GC_COOPERATING, MUTATOR_PARKED, MUTATOR_RUNNING};

/// One registered mutator thread.
pub struct Mutator {
    next: *mut Mutator,
    /// The owner's TLS slots, published by [`attach`] (null before).
    yield_flag: *const AtomicU8,
    state: *const AtomicU8,
    stack_top: *const *mut u8,
    /// `rok_tls_tlab_cursor` / `rok_tls_tlab_limit`: a collection empties
    /// every thread's TLAB (its unused tail is reclaimed by the sweep).
    tlab: [*mut usize; 2],
    /// `rok_tls_wb_log_cursor` / `_limit`: a collection retires every
    /// thread's remembered-set chunk.
    wb_log: [*mut usize; 2],
    /// `rok_tls_satb_cursor` / `_limit`: the final pause of a concurrent
    /// mark retires every thread's SATB chunk.
    satb: [*mut usize; 2],
    /// Extra root, alive while the record is registered: a spawned
    /// thread's closure environment (its only reference before the
    /// thread's own frames hold it).
    root: *mut u8,
    /// `rok_tls_worker_id` the spawner chose, written by [`attach`]
    /// (`u64::MAX`: keep the one `thread_init` allocated, then record it).
    worker_id: u64,
    /// The OS thread id, written by [`attach`] (0 before).
    #[cfg(windows)]
    os_id: u64,
    /// The owner's `rok_tls_held_locks` slot (null before [`attach`]).
    held: *mut usize,
}

/// The locks a thread holds outside the task executor ([`note_lock`]):
/// `(lock word buffer, kind)` pairs. A thread that is gone after a
/// failed REPL cell has its locks released ([`forget`]).
#[repr(C)]
pub struct Held {
    len: usize,
    cap: usize,
    items: *mut (usize, usize),
}

impl Held {
    /// The locks, oldest first.
    pub const fn items(&self) -> &[(usize, usize)] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: `len` initialized pairs.
        unsafe { core::slice::from_raw_parts(self.items, self.len) }
    }

    /// Drop the newest entry of `buf`; whether there was one.
    fn remove(&mut self, buf: usize) -> bool {
        let Some(i) = self.items().iter().rposition(|e| e.0 == buf) else {
            return false;
        };
        // SAFETY: `i < len`; the tail moves down by one.
        unsafe { ptr::copy(self.items.add(i + 1), self.items.add(i), self.len - i - 1) };
        self.len -= 1;
        true
    }
}

/// The record a `rok_tls_held_locks` slot points at (null: none yet).
///
/// # Safety
/// `slot` must be a live thread's slot.
const unsafe fn held_at(slot: *const usize) -> *mut Held {
    // SAFETY: caller contract.
    unsafe { slot.read() as *mut Held }
}

/// The calling thread now holds the lock whose words are GC buffer
/// `buf` (`kind` is the caller's).
pub fn note_lock(buf: usize, kind: usize) {
    let slot = tls::held_locks();
    // SAFETY: this thread's own slot.
    let mut h = unsafe { held_at(slot) };
    if h.is_null() {
        // SAFETY: plain allocation; published in our own slot.
        unsafe {
            h = libc::calloc(1, size_of::<Held>()).cast::<Held>();
            if h.is_null() {
                rokr_common::oom();
            }
            slot.write(h as usize);
        }
    }
    // SAFETY: our own record; only its owner grows it.
    unsafe {
        if (*h).len == (*h).cap {
            let cap = ((*h).cap * 2).max(4);
            let p = libc::realloc((*h).items.cast(), cap * size_of::<(usize, usize)>());
            if p.is_null() {
                rokr_common::oom();
            }
            (*h).items = p.cast();
            (*h).cap = cap;
        }
        (*h).items.add((*h).len).write((buf, kind));
        (*h).len += 1;
    }
}

/// A lock [`note_lock`] recorded is released: by its holder, or (a
/// guard handed on) by another thread.
pub fn drop_lock(buf: usize) {
    // SAFETY: this thread's own slot and record.
    unsafe {
        let h = held_at(tls::held_locks());
        if !h.is_null() && (*h).remove(buf) {
            return;
        }
    }
    lock();
    // SAFETY: registry guarded by the lock; records stay while their
    // thread is registered, and only the lock's holder edits another's.
    unsafe {
        let mut m = *WORLD.threads.get();
        while !m.is_null() {
            if !(*m).held.is_null() {
                let other = held_at((*m).held);
                if !other.is_null() && (*other).remove(buf) {
                    break;
                }
            }
            m = (*m).next;
        }
    }
    unlock();
}

/// With the world stopped by the caller: whether no other thread is
/// stopped at a safepoint while it holds a lock (inside its critical
/// section). One blocked in a call while it holds one counts as quiet.
pub fn quiet() -> bool {
    let me = tls::mutator_state().as_ptr().cast_const();
    // SAFETY: the caller holds the lock (it stopped the world); TLS slots
    // and records stay valid while their thread is registered.
    unsafe {
        let mut m = *WORLD.threads.get();
        while !m.is_null() {
            let s = (*m).state;
            if !s.is_null() && s.cast::<u8>() != me && !(*m).held.is_null() {
                let h = held_at((*m).held);
                if !h.is_null() && (*h).len > 0 && (*s).load(Ordering::SeqCst) != MUTATOR_PARKED {
                    return false;
                }
            }
            m = (*m).next;
        }
    }
    true
}

struct World {
    lock: sys::Lock,
    cond: sys::Cond,
    threads: UnsafeCell<*mut Mutator>,
}

// SAFETY: `threads` is only touched with `lock` held.
unsafe impl Sync for World {}

static WORLD: World = World {
    lock: sys::Lock::new(),
    cond: sys::Cond::new(),
    threads: UnsafeCell::new(ptr::null_mut()),
};

/// A stop-the-world pause is requested or running.
static STOP: AtomicBool = AtomicBool::new(false);

/// The process is exiting ([`halt`]): no thread runs rok code again.
static HALT: AtomicBool = AtomicBool::new(false);

/// `rok_runtime_shutdown`: no other thread runs rok code
/// again.
///
/// The worker pool and threads still running stop for good at their
/// next safepoint or world-lock acquisition; one inside runtime or C
/// code finishes that first, so it releases the locks the exit path
/// needs. The caller is done with rok code and counts as stopped.
pub fn halt() {
    raw_lock();
    HALT.store(true, Ordering::SeqCst);
    // SAFETY: registry guarded by the lock.
    unsafe {
        let mut m = *WORLD.threads.get();
        while !m.is_null() {
            if !(*m).yield_flag.is_null() {
                (*(*m).yield_flag).fetch_or(tls::YIELD_STOP, Ordering::SeqCst);
            }
            m = (*m).next;
        }
    }
    tls::mutator_state().store(MUTATOR_PARKED, Ordering::SeqCst);
    unlock();
}

fn raw_lock() {
    WORLD.lock.lock();
}

/// Into (`+1`) or out of (`-1`) a `#[no_safepoint]` call (debug builds).
#[cfg(debug_assertions)]
pub fn no_safepoint(step: i8) {
    let d = tls::no_safepoint();
    // SAFETY: this thread's own 1-byte slot.
    unsafe { d.write(d.read().wrapping_add_signed(step)) };
}

/// Fail if a `#[no_safepoint]` call is about to stop this thread.
///
/// A thread stops for a collection only at a safepoint the compiler
/// counts or at a back-edge poll: the poll's yield path is
/// the only place locals only polls see are rooted.
pub fn may_stop(what: &[u8]) {
    // SAFETY: this thread's own 1-byte slot.
    #[cfg(debug_assertions)]
    if unsafe { tls::no_safepoint().read() } != 0 {
        rokr_common::fatal(what);
    }
    let _ = what;
}

/// Release the world lock.
pub fn unlock() {
    WORLD.lock.unlock();
}

fn wait() {
    WORLD.cond.wait(&WORLD.lock);
}

fn broadcast() {
    WORLD.cond.broadcast();
}

/// With the lock held: if another thread is stopping the world, park
/// here (state `GC_COOPERATING`, our frames are its roots) until it is
/// done.
fn cooperate() {
    if !STOP.load(Ordering::Relaxed) {
        return;
    }
    may_stop(b"a #[no_safepoint] runtime call waited for a collection");
    let state = tls::mutator_state();
    let prev = state.load(Ordering::Relaxed);
    state.store(MUTATOR_GC_COOPERATING, Ordering::Release);
    broadcast();
    while STOP.load(Ordering::Relaxed) {
        wait();
    }
    state.store(prev, Ordering::Release);
}

/// [`lock`] for an `atexit` handler: after [`halt`] it takes the lock
/// instead of parking for good.
pub fn lock_at_exit() {
    if HALT.load(Ordering::SeqCst) {
        raw_lock();
    } else {
        lock();
    }
}

/// Take the world lock as a running mutator (heap access, registry
/// changes). Cooperates with a pending stop first.
pub fn lock() {
    raw_lock();
    if HALT.load(Ordering::Relaxed) {
        unlock();
        sys::park_forever();
    }
    cooperate();
}

/// `rok_runtime_yield`: the slow path of a safepoint poll.
pub fn safepoint() {
    may_stop(b"a #[no_safepoint] runtime call reached a safepoint");
    if HALT.load(Ordering::Relaxed) {
        sys::park_forever();
    }
    lock();
    // Under the lock, as a stopper raises the flag: cleared outside it,
    // a request raised just after this thread's pause would be lost and
    // its next `enter_native` would not wake that stopper.
    tls::yield_flag().fetch_and(!tls::YIELD_STOP, Ordering::Relaxed);
    unlock();
}

/// Before a call that may block: this thread counts as stopped until
/// [`leave_native`].
///
/// Join, sleep, lock wait, any `extern "C"` call. It must not touch GC
/// objects in between except reading words its own frames keep alive.
///
/// Lock-free unless a stop is pending: [`stop`] raises the yield flag
/// before reading the state and this writes the state before reading
/// the flag (both `SeqCst`), so one side always sees the other.
pub fn enter_native() {
    may_stop(b"a #[no_safepoint] runtime call blocked");
    tls::mutator_state().store(MUTATOR_PARKED, Ordering::SeqCst);
    if tls::yield_flag().load(Ordering::SeqCst) != 0 {
        raw_lock();
        broadcast();
        unlock();
    }
}

/// Back from a blocking call: wait out a running stop, then run again.
///
/// Lock-free unless a stop is pending (the same handshake as
/// [`enter_native`]: a stopper that read `RUNNING` raised our flag
/// first, one that read `PARKED` too).
pub fn leave_native() {
    let state = tls::mutator_state();
    state.store(MUTATOR_RUNNING, Ordering::SeqCst);
    if tls::yield_flag().load(Ordering::SeqCst) == 0 {
        return;
    }
    state.store(MUTATOR_PARKED, Ordering::SeqCst);
    raw_lock();
    broadcast();
    while STOP.load(Ordering::Relaxed) {
        wait();
    }
    state.store(MUTATOR_RUNNING, Ordering::Release);
    // Halted: the next safepoint stops this thread.
    if !HALT.load(Ordering::Relaxed) {
        tls::yield_flag().fetch_and(!tls::YIELD_STOP, Ordering::Relaxed);
    }
    unlock();
}

/// Register a thread that is about to be spawned, keeping `root` alive
/// until it detaches. It counts as stopped until it [`attach`]es.
pub fn register(root: *mut u8, worker_id: u64) -> *mut Mutator {
    // SAFETY: plain allocation.
    let m = unsafe { libc::malloc(size_of::<Mutator>()) }.cast::<Mutator>();
    if m.is_null() {
        rokr_common::oom();
    }
    lock();
    // SAFETY: `m` is fresh; the registry is guarded by the lock.
    unsafe {
        m.write(Mutator {
            next: *WORLD.threads.get(),
            yield_flag: ptr::null(),
            state: ptr::null(),
            stack_top: ptr::null(),
            tlab: [ptr::null_mut(); 2],
            wb_log: [ptr::null_mut(); 2],
            satb: [ptr::null_mut(); 2],
            root,
            worker_id,
            #[cfg(windows)]
            os_id: 0,
            held: ptr::null_mut(),
        });
        *WORLD.threads.get() = m;
    }
    unlock();
    m
}

/// On the registered thread itself (TLS already initialized): publish
/// its TLS slots and start running as a mutator.
///
/// # Safety
/// `m` must come from [`register`] and not be attached or unregistered.
pub unsafe fn attach(m: *mut Mutator) {
    raw_lock();
    while STOP.load(Ordering::Relaxed) {
        wait();
    }
    tls::mutator_state().store(MUTATOR_RUNNING, Ordering::Release);
    // SAFETY: `m` is registered and only its owner writes these fields,
    // under the lock the collector reads them with.
    unsafe {
        (*m).yield_flag = tls::yield_flag();
        (*m).state = tls::mutator_state();
        (*m).stack_top = tls::shadow_stack_top();
        (*m).tlab = tls::tlab();
        (*m).wb_log = tls::wb_log();
        (*m).satb = tls::satb_log();
        (*m).held = tls::held_locks();
        #[cfg(windows)]
        {
            (*m).os_id = u64::from(rokr_common::win::GetCurrentThreadId());
        }
        if (*m).worker_id == u64::MAX {
            (*m).worker_id = tls::worker_id().read();
        } else {
            tls::worker_id().write((*m).worker_id);
        }
    }
    unlock();
}

/// Register and attach the calling thread (the main thread, or a thread
/// that calls `rok_thread_init` itself). No-op if already registered.
pub fn attach_current() {
    raw_lock();
    let registered = !find_current().is_null();
    unlock();
    if !registered {
        // SAFETY: a fresh record.
        unsafe { attach(register(ptr::null_mut(), u64::MAX)) };
    }
}

/// With the lock held: this thread's record, or null.
fn find_current() -> *mut Mutator {
    let me = tls::shadow_stack_top().cast_const();
    // SAFETY: the registry is guarded by the caller's lock.
    unsafe {
        let mut m = *WORLD.threads.get();
        while !m.is_null() && (*m).stack_top != me {
            m = (*m).next;
        }
        m
    }
}

/// Remove the calling thread from the registry (`rok_thread_shutdown`).
/// It must not touch GC objects afterwards.
pub fn detach() {
    lock();
    crate::retire_own_logs();
    let me = find_current();
    // SAFETY: registry guarded by the lock; `me` came from `register`.
    unsafe {
        let mut link: *mut *mut Mutator = WORLD.threads.get();
        while !(*link).is_null() {
            if *link == me {
                *link = (*me).next;
                libc::free(me.cast());
                break;
            }
            link = &raw mut (**link).next;
        }
    }
    unlock();
    // SAFETY: our own slot and record, no longer reachable by others
    // (empty: guards drop before a thread ends).
    unsafe {
        let h = held_at(tls::held_locks());
        if !h.is_null() {
            tls::held_locks().write(0);
            libc::free((*h).items.cast());
            libc::free(h.cast());
        }
    }
}

/// With the world stopped by the caller: the OS ids of the other
/// attached threads into `out` (as many as fit); how many there are.
#[cfg(windows)]
pub fn other_thread_ids(out: &mut [u64]) -> usize {
    let me = tls::shadow_stack_top().cast_const();
    let mut n = 0;
    // SAFETY: the caller holds the lock (it stopped the world).
    unsafe {
        let mut m = *WORLD.threads.get();
        while !m.is_null() {
            if (*m).os_id != 0 && (*m).stack_top != me {
                if let Some(o) = out.get_mut(n) {
                    *o = (*m).os_id;
                }
                n += 1;
            }
            m = (*m).next;
        }
    }
    n
}

/// Drop the records of threads that no longer run.
///
/// Every other one (`lost` none: a forked copy has only the caller), or
/// those whose OS ids `lost` lists. `gone` gets each dropped thread's
/// worker id and the locks it held.
pub fn forget(lost: Option<&[u64]>, mut gone: impl FnMut(u64, &[(usize, usize)])) {
    let _ = &lost;
    lock();
    let me = find_current();
    // SAFETY: registry guarded by the lock; records came from `register`.
    unsafe {
        let mut link: *mut *mut Mutator = WORLD.threads.get();
        while !(*link).is_null() {
            let m = *link;
            #[cfg(windows)]
            let dead = m != me && lost.is_none_or(|l| l.contains(&(*m).os_id));
            #[cfg(not(windows))]
            let dead = m != me;
            if dead {
                *link = (*m).next;
                let h = if (*m).held.is_null() {
                    ptr::null_mut()
                } else {
                    held_at((*m).held)
                };
                gone((*m).worker_id, if h.is_null() { &[] } else { (*h).items() });
                if !h.is_null() {
                    libc::free((*h).items.cast());
                    libc::free(h.cast());
                }
                libc::free(m.cast());
            } else {
                link = &raw mut (*m).next;
            }
        }
    }
    unlock();
}

/// Unregister a record whose thread never started (spawn failed).
///
/// # Safety
/// `m` must come from [`register`] and its thread never have started.
pub unsafe fn unregister(m: *mut Mutator) {
    lock();
    // SAFETY: as in `detach`.
    unsafe {
        let mut link: *mut *mut Mutator = WORLD.threads.get();
        while !(*link).is_null() {
            if *link == m {
                *link = (*m).next;
                libc::free(m.cast());
                break;
            }
            link = &raw mut (**link).next;
        }
    }
    unlock();
}

/// With the lock held (taken by [`lock`]): stop every other registered
/// mutator. Returns once all of them are out of `RUNNING`.
pub fn stop() {
    STOP.store(true, Ordering::Release);
    let me = tls::mutator_state().as_ptr().cast_const();
    loop {
        let mut running = false;
        // SAFETY: registry guarded by the lock; published TLS slots stay
        // valid while their thread is registered.
        unsafe {
            let mut m = *WORLD.threads.get();
            while !m.is_null() {
                let s = (*m).state;
                if !s.is_null() && s.cast::<u8>() != me {
                    // `SeqCst`: the handshake of `enter_native`.
                    (*(*m).yield_flag).fetch_or(tls::YIELD_STOP, Ordering::SeqCst);
                    if (*s).load(Ordering::SeqCst) == MUTATOR_RUNNING {
                        running = true;
                    }
                }
                m = (*m).next;
            }
        }
        if !running {
            return;
        }
        wait();
    }
}

/// With the world stopped: empty every registered thread's TLAB (and
/// the caller's, registered or not), so the sweep may hand its unused
/// lines to anyone.
pub fn reset_tlabs() {
    // SAFETY: as in `stop`; the slots are only written by their owner,
    // which is stopped, and by us.
    unsafe {
        let mut m = *WORLD.threads.get();
        while !m.is_null() {
            for p in (*m).tlab {
                if !p.is_null() {
                    p.write(0);
                }
            }
            m = (*m).next;
        }
        for p in tls::tlab() {
            p.write(0);
        }
    }
}

/// With the world stopped: `f` on the wb log slots of every registered
/// thread and the caller's (registered or not).
pub fn for_each_wb_log(mut f: impl FnMut([*mut usize; 2])) {
    // SAFETY: as in `reset_tlabs`.
    unsafe {
        let mut m = *WORLD.threads.get();
        while !m.is_null() {
            if !(*m).wb_log[0].is_null() {
                f((*m).wb_log);
            }
            m = (*m).next;
        }
    }
    f(tls::wb_log());
}

/// With the world stopped: `f` on the SATB log slots of every registered
/// thread and the caller's.
pub fn for_each_satb(mut f: impl FnMut([*mut usize; 2])) {
    // SAFETY: as in `reset_tlabs`.
    unsafe {
        let mut m = *WORLD.threads.get();
        while !m.is_null() {
            if !(*m).satb[0].is_null() {
                f((*m).satb);
            }
            m = (*m).next;
        }
    }
    f(tls::satb_log());
}

/// With the lock held: end the pause and wake every parked mutator.
pub fn resume() {
    STOP.store(false, Ordering::Release);
    // SAFETY: as in `stop`.
    unsafe {
        let mut m = *WORLD.threads.get();
        while !m.is_null() {
            if !(*m).yield_flag.is_null() {
                (*(*m).yield_flag).fetch_and(!tls::YIELD_STOP, Ordering::Relaxed);
            }
            m = (*m).next;
        }
    }
    broadcast();
}

/// With the world stopped: call `f` with the address of every root word
/// of every registered thread (shadow-stack slots, spawn roots).
#[allow(clippy::cast_ptr_alignment)] // frames and slots are word-aligned
pub fn for_each_root(mut f: impl FnMut(*mut u8)) {
    #[repr(C)]
    struct Frame {
        prev: *mut Frame,
        count: usize,
        // `slots: [*mut usize; count]` follows.
    }
    // SAFETY: every frame on a stopped thread's chain was linked by
    // compiled code and stays live until its function returns, which
    // cannot happen while the thread is stopped; each slot points at a
    // word inside such a function's frame.
    unsafe {
        let mut m = *WORLD.threads.get();
        while !m.is_null() {
            if !(*m).root.is_null() {
                f((*m).root);
            }
            if !(*m).stack_top.is_null() {
                let mut fr = (*(*m).stack_top).cast::<Frame>();
                while !fr.is_null() {
                    let slots = fr.add(1).cast::<*const usize>();
                    for i in 0..(*fr).count {
                        f(**slots.add(i) as *mut u8);
                    }
                    fr = (*fr).prev;
                }
            }
            m = (*m).next;
        }
        let g = &*GLOBAL_ROOTS.0.get();
        for i in 0..g.len {
            f(*(*g.words.add(i)) as *mut u8);
        }
    }
}

/// Addresses of words outside any stack that hold GC references
/// (statics), scanned by every collection.
struct GlobalRoots {
    words: *mut *const usize,
    len: usize,
    cap: usize,
}

struct GlobalRootsCell(UnsafeCell<GlobalRoots>);
// SAFETY: only touched with the world lock held.
unsafe impl Sync for GlobalRootsCell {}

static GLOBAL_ROOTS: GlobalRootsCell = GlobalRootsCell(UnsafeCell::new(GlobalRoots {
    words: ptr::null_mut(),
    len: 0,
    cap: 0,
}));

/// Make the word at `word` a root for the rest of the process.
pub fn add_root(word: *const usize) {
    lock();
    // SAFETY: guarded by the lock; realloc of our own buffer.
    unsafe {
        let g = &mut *GLOBAL_ROOTS.0.get();
        if g.len == g.cap {
            let cap = (g.cap * 2).max(16);
            let p = libc::realloc(g.words.cast(), cap * size_of::<*const usize>());
            if p.is_null() {
                rokr_common::oom();
            }
            g.words = p.cast();
            g.cap = cap;
        }
        *g.words.add(g.len) = word;
        g.len += 1;
    }
    unlock();
}
