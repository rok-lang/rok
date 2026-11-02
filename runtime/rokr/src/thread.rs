//! OS threads and the one address-keyed wait
//! queue every blocking wait in the runtime and in `rok::core::sync`
//! goes through. wasm32 has one thread: no spawn, and a
//! wait that only another thread could end is a deadlock.

use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicI32, AtomicU8, Ordering};

use rokr_common::object_header::HEADER_SIZE;
use rokr_common::{sys, tls};
use rokr_gc::world;

/// A rok `fn()` value: `{code, env}`; `code` takes the env first.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RokFn {
    code: Option<unsafe extern "C" fn(*mut u8)>,
    env: *mut u8,
}

#[cfg(not(rok_solo))]
struct Start {
    me: *mut world::Mutator,
    f: RokFn,
}

/// The smallest stack `pthread_attr_setstacksize` takes (wasi-libc's is
/// not in the `libc` crate).
#[cfg(unix)]
const STACK_MIN: usize = libc::PTHREAD_STACK_MIN;
#[cfg(rok_wasi_threads)]
const STACK_MIN: usize = 2048;

/// Rok frames are large in debug builds and the macOS default (512 KiB)
/// is small.
#[cfg(not(rok_solo))]
const DEFAULT_STACK: usize = 8 << 20;

#[cfg(any(unix, rok_wasi_threads))]
extern "C" fn thread_main(arg: *mut libc::c_void) -> *mut libc::c_void {
    run(arg.cast());
    ptr::null_mut()
}

#[cfg(windows)]
unsafe extern "system" fn thread_main(arg: *mut core::ffi::c_void) -> u32 {
    run(arg.cast());
    0
}

#[cfg(not(rok_solo))]
fn run(arg: *mut Start) {
    // SAFETY: `arg` is the `Start` that `rok_thread_spawn` leaked to us.
    let s = unsafe { ptr::read(arg) };
    // SAFETY: allocated with `malloc` in `rok_thread_spawn`.
    unsafe { libc::free(arg.cast()) };
    tls::thread_init();
    crate::stack::thread_start();
    // SAFETY: registered for this thread by `rok_thread_spawn`.
    unsafe { world::attach(s.me) };
    if let Some(code) = s.f.code {
        // SAFETY: a rok `fn()` code pointer called with its own env.
        unsafe { code(s.f.env) };
    }
    thread_exit();
}

/// Leave the mutator registry; the thread must not touch GC objects
/// afterwards.
pub fn thread_exit() {
    rok_task_worker_clear();
    world::detach();
    crate::stack::thread_exit();
    // SAFETY: this thread's own slot.
    let [p, _] = unsafe { tls::thread_name().read() };
    #[cfg(not(rok_solo))]
    if p != 0 && p != MAIN_NAME.as_ptr() as usize {
        // SAFETY: allocated by `rok_thread_set_name`.
        unsafe { libc::free(p as *mut libc::c_void) };
    }
    #[cfg(rok_solo)]
    let _ = p;
    // SAFETY: as above.
    unsafe {
        tls::thread_name().write([0, 0]);
        tls::attached().write(0);
    }
}

/// A call into rok on a thread rok did not start: attach it as a
/// mutator, and leave the registry when the thread ends. It keeps its
/// own stack setup (no guard range, no alternate signal stack).
#[cfg(not(target_family = "wasm"))]
pub fn adopt() {
    tls::thread_init();
    world::attach_current();
    // SAFETY: this thread's own slot.
    unsafe { tls::attached().write(tls::ATTACHED_CALLBACK) };
    on_thread_end();
}

#[cfg(target_family = "wasm")]
pub fn adopt() {
    rokr_common::fatal(
        b"rok: callback on a thread rok does not know; call rok_thread_init() first",
    );
}

/// At the end of an adopted thread, while its TLS is still there.
#[cfg(not(target_family = "wasm"))]
fn adopted_end() {
    // SAFETY: this thread's own slot.
    if unsafe { tls::attached().read() } != tls::ATTACHED_CALLBACK {
        return; // `rok_thread_shutdown` came first
    }
    // Between callbacks the thread is in C, `PARKED`.
    if tls::mutator_state().load(Ordering::Relaxed) == tls::MUTATOR_PARKED {
        world::leave_native();
    }
    thread_exit();
}

/// Run `adopted_end` when this thread ends: `_tlv_atexit` on Apple (a
/// pthread key destructor may run after the TLV block is freed).
#[cfg(target_vendor = "apple")]
fn on_thread_end() {
    unsafe extern "C" {
        #[link_name = "_tlv_atexit"]
        fn tlv_atexit(f: unsafe extern "C" fn(*mut libc::c_void), arg: *mut libc::c_void);
    }
    unsafe extern "C" fn end(_: *mut libc::c_void) {
        adopted_end();
    }
    // SAFETY: registers a plain function for this thread's exit.
    unsafe { tlv_atexit(end, ptr::null_mut()) };
}

/// A pthread key whose destructor runs `adopted_end` (static TLS lives
/// until after key destructors).
#[cfg(all(unix, not(target_vendor = "apple")))]
fn on_thread_end() {
    use core::sync::atomic::AtomicUsize;
    static KEY: AtomicUsize = AtomicUsize::new(usize::MAX);
    unsafe extern "C" fn end(_: *mut libc::c_void) {
        adopted_end();
    }
    let mut key = KEY.load(Ordering::Acquire);
    if key == usize::MAX {
        let mut k: libc::pthread_key_t = 0;
        // SAFETY: `k` is writable; `end` is a plain function.
        if unsafe { libc::pthread_key_create(&raw mut k, Some(end)) } != 0 {
            rokr_common::fatal(b"rok: no pthread key left to attach a foreign thread");
        }
        key =
            match KEY.compare_exchange(usize::MAX, k as usize, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => k as usize,
                Err(won) => {
                    // SAFETY: ours, never set.
                    unsafe { libc::pthread_key_delete(k) };
                    won
                }
            };
    }
    // SAFETY: a live key; any non-null value runs the destructor.
    unsafe { libc::pthread_setspecific(key as libc::pthread_key_t, ptr::dangling()) };
}

/// An FLS slot whose callback runs `adopted_end` (FLS callbacks run
/// before the thread's static TLS is freed).
#[cfg(windows)]
fn on_thread_end() {
    use core::sync::atomic::AtomicU32;
    use rokr_common::win;
    static SLOT: AtomicU32 = AtomicU32::new(win::FLS_OUT_OF_INDEXES);
    unsafe extern "system" fn end(_: *mut core::ffi::c_void) {
        adopted_end();
    }
    let mut slot = SLOT.load(Ordering::Acquire);
    if slot == win::FLS_OUT_OF_INDEXES {
        // SAFETY: `end` is a plain function.
        let s = unsafe { win::FlsAlloc(Some(end)) };
        if s == win::FLS_OUT_OF_INDEXES {
            rokr_common::fatal(b"rok: no FLS slot left to attach a foreign thread");
        }
        slot = match SLOT.compare_exchange(
            win::FLS_OUT_OF_INDEXES,
            s,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => s,
            Err(won) => {
                // SAFETY: ours, never set.
                unsafe { win::FlsFree(s) };
                won
            }
        };
    }
    // SAFETY: a live slot; any non-null value runs the callback.
    unsafe { win::FlsSetValue(slot, ptr::dangling_mut()) };
}

static MAIN_NAME: [u8; 4] = *b"main";

/// Name the calling (main) thread `main` in reports. Its OS name stays:
/// on Linux it is the process's.
pub fn name_main() {
    // SAFETY: this thread's own slot.
    unsafe { tls::thread_name().write([MAIN_NAME.as_ptr() as usize, MAIN_NAME.len()]) };
}

/// The calling thread's name in reports: the one given at spawn, `main`,
/// else `<unnamed>`. Async-signal-safe.
pub fn name() -> &'static [u8] {
    // SAFETY: this thread's own slot; a name lives until `thread_exit`.
    unsafe {
        let [p, n] = tls::thread_name().read();
        if p == 0 {
            b"<unnamed>"
        } else {
            core::slice::from_raw_parts(p as *const u8, n)
        }
    }
}

/// `thread::Builder::name`, called on the new thread before its closure:
/// the name in reports, also set as the OS thread name.
#[cfg(not(rok_solo))]
#[unsafe(no_mangle)]
pub extern "C" fn rok_thread_set_name(name: crate::RokStr) {
    let b = name.bytes();
    // SAFETY: a fresh block of `len + 1` bytes; this thread's own slot.
    unsafe {
        let p = libc::malloc(b.len() + 1).cast::<u8>();
        if p.is_null() {
            rokr_common::oom();
        }
        ptr::copy_nonoverlapping(b.as_ptr(), p, b.len());
        p.add(b.len()).write(0);
        let [old, _] = tls::thread_name().read();
        if old != 0 && old != MAIN_NAME.as_ptr() as usize {
            libc::free(old as *mut libc::c_void);
        }
        tls::thread_name().write([p as usize, b.len()]);
    }
    set_os_name(b);
}

/// No spawned threads on wasm32.
#[cfg(rok_solo)]
#[unsafe(no_mangle)]
pub const extern "C" fn rok_thread_set_name(_name: crate::RokStr) {}

/// The OS thread name: cut at a char boundary to what the OS takes
/// (Linux 15 bytes, macOS 63).
#[cfg(unix)]
fn set_os_name(b: &[u8]) {
    let mut c = [0u8; 64];
    let max = if cfg!(target_vendor = "apple") {
        63
    } else {
        15
    };
    let mut n = b.len().min(max);
    while n < b.len() && n > 0 && b[n] & 0xC0 == 0x80 {
        n -= 1;
    }
    c[..n].copy_from_slice(&b[..n]);
    // SAFETY: `c` is NUL-terminated.
    unsafe {
        #[cfg(target_vendor = "apple")]
        libc::pthread_setname_np(c.as_ptr().cast());
        #[cfg(not(target_vendor = "apple"))]
        libc::pthread_setname_np(libc::pthread_self(), c.as_ptr().cast());
    }
}

#[cfg(windows)]
fn set_os_name(b: &[u8]) {
    use rokr_common::win;
    let mut w = rokr_common::wtf8::wide(b);
    // SAFETY: `w` is NUL-terminated and outlives the call.
    unsafe { win::SetThreadDescription(win::GetCurrentThread(), w.ptr()) };
}

/// wasi-threads threads have no OS name.
#[cfg(rok_wasi_threads)]
const fn set_os_name(_b: &[u8]) {}

/// A fresh thread id for [`rok_thread_spawn`].
#[unsafe(no_mangle)]
pub extern "C" fn rok_thread_new_id() -> u64 {
    tls::new_worker_id()
}

/// Start an OS thread running the rok closure `f` (`stack_size` bytes of
/// stack, 0 = default) whose `rok_tls_worker_id` is `id` (from
/// [`rok_thread_new_id`]). Returns an opaque join handle, or 0 if the
/// thread could not be created. `f`'s environment stays a GC root until
/// the thread ends. The ABI has `rok_thread_spawn(closure: Ptr<u8>)`; a rok
/// closure crosses as its `{code, env}` pair.
#[cfg(not(rok_solo))]
#[unsafe(no_mangle)]
pub extern "C" fn rok_thread_spawn(f: RokFn, stack_size: usize, id: u64) -> usize {
    let me = world::register(f.env, id);
    // SAFETY: plain allocation.
    let start = unsafe { libc::malloc(size_of::<Start>()) }.cast::<Start>();
    if start.is_null() {
        rokr_common::oom();
    }
    // SAFETY: fresh allocation.
    unsafe { start.write(Start { me, f }) };
    let handle = spawn_os(start, stack_size);
    if handle == 0 {
        // SAFETY: the thread did not start: `start` and `me` are still ours.
        unsafe {
            libc::free(start.cast());
            world::unregister(me);
        }
    }
    handle
}

/// No threads on wasm32: the spawn fails.
#[cfg(rok_solo)]
#[unsafe(no_mangle)]
pub const extern "C" fn rok_thread_spawn(_f: RokFn, _stack_size: usize, _id: u64) -> usize {
    0
}

/// An OS thread running [`run`]`(start)`: its join handle, or 0.
#[cfg(any(unix, rok_wasi_threads))]
fn spawn_os(start: *mut Start, stack_size: usize) -> usize {
    // SAFETY: the attr / thread calls get valid out-pointers.
    unsafe {
        let mut attr: libc::pthread_attr_t = core::mem::zeroed();
        libc::pthread_attr_init(&mut attr);
        let size = if stack_size == 0 {
            DEFAULT_STACK
        } else {
            stack_size.max(STACK_MIN)
        };
        libc::pthread_attr_setstacksize(&mut attr, size);
        let mut tid: libc::pthread_t = core::mem::zeroed();
        let rc = libc::pthread_create(&mut tid, &attr, thread_main, start.cast());
        libc::pthread_attr_destroy(&mut attr);
        if rc != 0 { 0 } else { tid as usize }
    }
}

/// An OS thread running [`run`]`(start)`: its handle, or 0.
#[cfg(windows)]
fn spawn_os(start: *mut Start, stack_size: usize) -> usize {
    use rokr_common::win;
    let size = if stack_size == 0 {
        DEFAULT_STACK
    } else {
        stack_size
    };
    // SAFETY: plain thread creation; `start` stays valid until `run`
    // takes it.
    unsafe {
        win::CreateThread(
            ptr::null_mut(),
            size,
            thread_main,
            start.cast(),
            win::STACK_SIZE_PARAM_IS_A_RESERVATION,
            ptr::null_mut(),
        ) as usize
    }
}

/// Wait for the thread behind `handle` to finish. 0 on success.
#[unsafe(no_mangle)]
pub extern "C" fn rok_thread_join(handle: usize) -> i32 {
    crate::lane::block_begin();
    // SAFETY: `handle` came from `rok_thread_spawn`.
    #[cfg(any(unix, rok_wasi_threads))]
    let rc = unsafe { libc::pthread_join(handle as libc::pthread_t, ptr::null_mut()) };
    // SAFETY: as above; the join consumes the handle.
    #[cfg(windows)]
    let rc = unsafe {
        use rokr_common::win;
        let h = handle as win::HANDLE;
        let r = win::WaitForSingleObject(h, win::INFINITE);
        win::CloseHandle(h);
        if r == win::WAIT_OBJECT_0 {
            0
        } else {
            sys::errno()
        }
    };
    // No handle exists to join.
    #[cfg(rok_solo)]
    let rc = {
        let _ = handle;
        libc::ESRCH
    };
    crate::lane::block_end();
    rc
}

/// Let the thread behind `handle` release its OS resources when it ends;
/// the handle is no longer joinable. 0 on success.
#[unsafe(no_mangle)]
#[cfg_attr(rok_solo, allow(clippy::missing_const_for_fn))]
pub extern "C" fn rok_thread_detach(handle: usize) -> i32 {
    // SAFETY: `handle` came from `rok_thread_spawn` and was not joined.
    #[cfg(any(unix, rok_wasi_threads))]
    unsafe {
        libc::pthread_detach(handle as libc::pthread_t)
    }
    // SAFETY: as above.
    #[cfg(windows)]
    unsafe {
        i32::from(rokr_common::win::CloseHandle(handle as rokr_common::win::HANDLE) == 0)
    }
    #[cfg(rok_solo)]
    {
        let _ = handle;
        libc::ESRCH
    }
}

/// Online CPUs (at least 1): the executor's width. 0: no thread can be
/// started (wasm32 without threads), the executor runs solo.
#[unsafe(no_mangle)]
#[cfg_attr(rok_solo, allow(clippy::missing_const_for_fn))]
pub extern "C" fn rok_cpu_count() -> usize {
    // SAFETY: plain sysconf query.
    #[cfg(unix)]
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) } as usize;
    // WASI cannot tell how many cores run its threads.
    #[cfg(rok_wasi_threads)]
    let n = 4;
    // SAFETY: plain query; 0xFFFF = every processor group.
    #[cfg(windows)]
    let n = unsafe { rokr_common::win::GetActiveProcessorCount(0xFFFF) } as usize;
    #[cfg(not(rok_solo))]
    return n.max(1);
    #[cfg(rok_solo)]
    0
}

/// The executor's pool size: `ROKR_WORKERS` when set to a
/// positive number, else the online CPUs; 0 without threads.
#[unsafe(no_mangle)]
pub extern "C" fn rok_sched_workers() -> usize {
    let n = rok_cpu_count();
    match rokr_common::env_num(c"ROKR_WORKERS", true) {
        Some(w) if n > 0 => usize::try_from(w).unwrap_or(usize::MAX),
        _ => n,
    }
}

/// The executor's options: bit 0 thread-per-core
/// (`ROKR_THREAD_PER_CORE=1`), bit 1 dynamic worker count
/// (`ROKR_DYNAMIC_WORKERS=1`).
#[unsafe(no_mangle)]
pub extern "C" fn rok_sched_mode() -> u32 {
    let on = rokr_common::env_flag;
    u32::from(on(c"ROKR_THREAD_PER_CORE")) | (u32::from(on(c"ROKR_DYNAMIC_WORKERS")) << 1)
}

/// This thread's worker id (`rok_tls_worker_id`), the `ThreadId`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_thread_id() -> u64 {
    // SAFETY: this thread's own slot.
    unsafe { tls::worker_id().read() }
}

/// Block the calling thread for `ns` nanoseconds (negative = 0).
#[unsafe(no_mangle)]
pub extern "C" fn rok_thread_sleep_ns(ns: i64) {
    crate::lane::block_begin();
    sleep(ns.max(0));
    crate::lane::block_end();
}

#[cfg(rok_solo)]
fn sleep(ns: i64) {
    sys::sleep_ns(ns as u64);
}

#[cfg(any(unix, rok_wasi_threads))]
fn sleep(ns: i64) {
    // SAFETY: a zeroed `timespec` is valid.
    let mut ts: libc::timespec = unsafe { core::mem::zeroed() };
    ts.tv_sec = (ns / 1_000_000_000) as _;
    ts.tv_nsec = (ns % 1_000_000_000) as _;
    // SAFETY: `ts` is valid for read and write (remaining time).
    while unsafe { libc::nanosleep(&ts, &mut ts) } != 0 {}
}

/// A high-resolution waitable timer (`Sleep` rounds to the system tick).
#[cfg(windows)]
fn sleep(ns: i64) {
    use rokr_common::win;
    // SAFETY: a private timer, waited on and closed here; a negative due
    // time is relative, in 100 ns units.
    unsafe {
        let t = win::CreateWaitableTimerExW(
            ptr::null_mut(),
            ptr::null(),
            win::CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            win::TIMER_ALL_ACCESS,
        );
        if t == 0 {
            rokr_common::fatal(b"rok runtime: CreateWaitableTimerExW failed");
        }
        let due = -((ns + 99) / 100).max(1);
        win::SetWaitableTimer(t, &due, 0, ptr::null_mut(), ptr::null_mut(), 0);
        win::WaitForSingleObject(t, win::INFINITE);
        win::CloseHandle(t);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_thread_yield_now() {
    // SAFETY: no preconditions.
    #[cfg(any(unix, target_os = "wasi"))]
    unsafe {
        libc::sched_yield();
    }
    // SAFETY: as above.
    #[cfg(windows)]
    unsafe {
        rokr_common::win::SwitchToThread();
    }
}

// This thread's task-executor worker (`rok::core::task`): a GC object
// held in `rok_tls_task_worker`, pinned while installed.

/// 1 if this thread is an executor worker.
#[unsafe(no_mangle)]
pub extern "C" fn rok_task_worker_present() -> i32 {
    i32::from(rok_task_worker_get() != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_task_worker_get() -> usize {
    // SAFETY: this thread's own slot.
    unsafe { tls::task_worker().read() }
}

/// Install `w` (a live GC object, or 0 to clear) as this thread's
/// worker, releasing the previous one.
#[unsafe(no_mangle)]
pub extern "C" fn rok_task_worker_set(w: usize) {
    let old = rok_task_worker_get();
    // SAFETY: the slot only holds pinned, hence live, objects; the
    // caller passes a live object or 0.
    unsafe {
        if old != 0 {
            rokr_gc::unpin(old as *mut u8);
        }
        if w != 0 {
            rokr_gc::pin(w as *mut u8);
        }
        tls::task_worker().write(w);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_task_worker_clear() {
    rok_task_worker_set(0);
}

// Wait queue: futex semantics over a fixed table of buckets keyed by
// address. Each bucket holds a list of its waiters (address + own
// condvar), so a wake reaches only the waiters of its word, and
// `wake_one` exactly one of them. Spurious wakeups are allowed (callers
// re-check). A waiter counts as stopped for the GC.

/// A thread blocked in [`wait_on`]: a node on its stack, linked into
/// its bucket while it waits.
#[cfg(not(rok_solo))]
struct Waiter {
    addr: usize,
    /// The waiting thread's worker id (`rok_tls_worker_id`), for
    /// `forget_waiters` (the REPL; no REPL on wasm32).
    #[cfg(not(target_family = "wasm"))]
    owner: u64,
    cond: sys::Cond,
    /// Unlinked by a waker (under the bucket lock).
    woken: bool,
    next: *mut Waiter,
}

#[cfg(not(rok_solo))]
struct Bucket {
    lock: sys::Lock,
    /// Waiters, oldest first (under `lock`).
    head: core::cell::UnsafeCell<*mut Waiter>,
}

// SAFETY: `head` is only touched under `lock`.
#[cfg(not(rok_solo))]
unsafe impl Sync for Bucket {}

#[cfg(not(rok_solo))]
static BUCKETS: [Bucket; 64] = [const {
    Bucket {
        lock: sys::Lock::new(),
        head: core::cell::UnsafeCell::new(ptr::null_mut()),
    }
}; 64];

#[cfg(not(rok_solo))]
fn bucket(addr: usize) -> &'static Bucket {
    &BUCKETS[(addr >> 2) % BUCKETS.len()]
}

#[cfg(not(rok_solo))]
impl Bucket {
    /// Unlink the first waiter on `addr` (`all`: every one) and signal
    /// it.
    ///
    /// # Safety
    /// The caller holds `lock`.
    unsafe fn wake(&self, addr: usize, all: bool) {
        // SAFETY: the list is ours under the lock; nodes stay alive
        // until their owner sees `woken` or unlinks itself (both under
        // the lock).
        unsafe {
            let mut link = self.head.get();
            while !(*link).is_null() {
                let w = *link;
                if (*w).addr != addr {
                    link = &raw mut (*w).next;
                    continue;
                }
                *link = (*w).next;
                (*w).woken = true;
                (*w).cond.broadcast();
                if !all {
                    return;
                }
            }
        }
    }

    /// Unlink `me` if a waker has not.
    ///
    /// # Safety
    /// The caller holds `lock`; `me` is its own node.
    unsafe fn leave(&self, me: *mut Waiter) {
        // SAFETY: as in `wake`.
        unsafe {
            if (*me).woken {
                return;
            }
            let mut link = self.head.get();
            while *link != me {
                link = &raw mut (**link).next;
            }
            *link = (*me).next;
        }
    }
}

/// A word a thread can wait on.
pub trait Futex {
    fn value(&self) -> i32;
}

impl Futex for AtomicI32 {
    fn value(&self) -> i32 {
        self.load(Ordering::SeqCst)
    }
}

impl Futex for AtomicU8 {
    fn value(&self) -> i32 {
        i32::from(self.load(Ordering::SeqCst))
    }
}

/// Block while `w` holds `expected`, at most `timeout_ns` (< 0: no
/// limit); may return spuriously.
#[cfg(rok_solo)]
pub fn wait_on<W: Futex>(w: &W, expected: i32, timeout_ns: i64) {
    if w.value() != expected {
        return;
    }
    if timeout_ns < 0 {
        rokr_common::fatal(b"fatal error: deadlock: the only thread waits for another");
    }
    crate::lane::block_begin();
    sys::sleep_ns(timeout_ns as u64);
    crate::lane::block_end();
}

/// Block while `w` holds `expected`, at most `timeout_ns` (< 0: no
/// limit); may return spuriously.
#[cfg(not(rok_solo))]
pub fn wait_on<W: Futex>(w: &W, expected: i32, timeout_ns: i64) {
    let addr = ptr::from_ref(w) as usize;
    let b = bucket(addr);
    let mut me = Waiter {
        addr,
        // SAFETY: this thread's own slot.
        #[cfg(not(target_family = "wasm"))]
        owner: unsafe { tls::worker_id().read() },
        cond: sys::Cond::new(),
        woken: false,
        next: ptr::null_mut(),
    };
    let me_p = &raw mut me;
    crate::lane::block_begin();
    b.lock.lock();
    if w.value() == expected {
        // SAFETY: the list is ours under the lock; `me` outlives its time
        // in the list (unlinked before we return).
        unsafe {
            // Append: wakes go oldest first.
            let mut link = b.head.get();
            while !(*link).is_null() {
                link = &raw mut (**link).next;
            }
            *link = me_p;
            if timeout_ns < 0 {
                (*me_p).cond.wait(&b.lock);
            } else {
                (*me_p).cond.wait_timeout(&b.lock, timeout_ns as u64);
            }
            b.leave(me_p);
        }
    }
    b.lock.unlock();
    crate::lane::block_end();
}

/// Unlink the waiters of threads that are gone (`gone(worker id)`): a
/// failed REPL cell's session restored with their entries, which live on
/// those threads' stacks.
#[cfg(not(target_family = "wasm"))]
pub fn forget_waiters(gone: impl Fn(u64) -> bool) {
    for b in &BUCKETS {
        b.lock.lock();
        // SAFETY: the list is ours under the lock; a gone thread's node is
        // never touched again.
        unsafe {
            let mut link = b.head.get();
            while !(*link).is_null() {
                if gone((**link).owner) {
                    *link = (**link).next;
                } else {
                    link = &raw mut (**link).next;
                }
            }
        }
        b.lock.unlock();
    }
}

/// Wake every thread blocked in [`wait_on`] (a wait may end spuriously:
/// each checks its word again). After a failed REPL cell, for threads
/// waiting on a lock the restore released.
#[cfg(not(target_family = "wasm"))]
pub fn wake_all_waiters() {
    for b in &BUCKETS {
        b.lock.lock();
        // SAFETY: the list is ours under the lock.
        unsafe {
            while !(*b.head.get()).is_null() {
                b.wake((**b.head.get()).addr, true);
            }
        }
        b.lock.unlock();
    }
}

/// Wake every thread blocked in [`wait_on`] on `w`.
pub fn wake<W: Futex>(w: &W) {
    wake_waiters(ptr::from_ref(w) as usize, true);
}

/// One thread: nobody waits.
#[cfg(rok_solo)]
const fn wake_waiters(_addr: usize, _all: bool) {}

#[cfg(not(rok_solo))]
fn wake_waiters(addr: usize, all: bool) {
    let b = bucket(addr);
    b.lock.lock();
    // SAFETY: the lock is held around the list walk.
    unsafe { b.wake(addr, all) };
    b.lock.unlock();
}

/// Element `index` of the `GcBuf<i32>` `buf`.
#[allow(clippy::cast_ptr_alignment)] // i32 buffer elements are 4-aligned
fn word(buf: *mut u8, index: usize) -> &'static AtomicI32 {
    // SAFETY: the caller's frame roots `buf`, so it is alive and stays in
    // place while this thread waits; element `index` is inside it.
    unsafe { &*buf.add(HEADER_SIZE + index * 4).cast::<AtomicI32>() }
}

/// Block while element `index` of the `GcBuf<i32>` `buf` equals
/// `expected`, at most `timeout_ns` (< 0: no limit), or until woken;
/// may return spuriously.
#[unsafe(no_mangle)]
pub extern "C" fn rok_sync_wait(buf: *mut u8, index: usize, expected: i32, timeout_ns: i64) {
    wait_on(word(buf, index), expected, timeout_ns);
}

/// Wake every thread blocked in `rok_sync_wait` on that element.
#[unsafe(no_mangle)]
pub extern "C" fn rok_sync_wake(buf: *mut u8, index: usize) {
    wake(word(buf, index));
}

/// Wake the longest-blocked thread in `rok_sync_wait` on that element.
#[unsafe(no_mangle)]
pub extern "C" fn rok_sync_wake_one(buf: *mut u8, index: usize) {
    wake_waiters(ptr::from_ref(word(buf, index)) as usize, false);
}

// Lazily initialized statics: compiled code checks the state byte
// (0 new, 1 running, 2 ready) and calls these around the initializer.

/// This thread's running initializers, innermost first: a malloc'd
/// list headed in `rok_tls_static_inits` (no limit on nesting or
/// concurrency).
struct Initializing {
    state: usize,
    next: *mut Initializing,
}

fn initializing_head() -> *mut Initializing {
    // SAFETY: this thread's own slot.
    unsafe { tls::static_inits().read() as *mut Initializing }
}

fn set_initializing_head(head: *mut Initializing) {
    // SAFETY: this thread's own slot.
    unsafe { tls::static_inits().write(head as usize) };
}

/// 1: the caller runs the initializer, then calls
/// [`rok_static_init_end`]. 0: it is ready (another thread may have
/// just finished it while this one waited). An initializer that reads
/// its own static panics at `at` (`at_len` bytes), the
/// location of the read.
#[unsafe(no_mangle)]
pub extern "C" fn rok_static_init_begin(
    state: *const AtomicU8,
    at: *const u8,
    at_len: usize,
) -> i32 {
    // SAFETY: compiled code passes its static's state byte.
    let s = unsafe { &*state };
    if s.compare_exchange(0, 1, Ordering::Acquire, Ordering::Acquire)
        .is_ok()
    {
        // SAFETY: malloc of one node, freed in `rok_static_init_end`.
        let node = unsafe { libc::malloc(size_of::<Initializing>()) }.cast::<Initializing>();
        if node.is_null() {
            rokr_common::oom();
        }
        // SAFETY: fresh node.
        unsafe {
            node.write(Initializing {
                state: state as usize,
                next: initializing_head(),
            });
        }
        set_initializing_head(node);
        return 1;
    }
    if s.load(Ordering::Acquire) != 2 {
        let mut n = initializing_head();
        let mut mine = false;
        while !n.is_null() {
            // SAFETY: nodes of this thread's list.
            unsafe {
                mine |= (*n).state == state as usize;
                n = (*n).next;
            }
        }
        if mine {
            let msg = b"static initializer reads its own static (recursive initialization)";
            // SAFETY: compiled code passes its location string.
            let at = unsafe { core::slice::from_raw_parts(at, at_len) };
            crate::panic_at(msg, Some(at));
        }
        while s.load(Ordering::Acquire) != 2 {
            wait_on(s, 1, -1);
        }
    }
    0
}

/// The initializer stored the value: publish it.
#[unsafe(no_mangle)]
pub extern "C" fn rok_static_init_end(state: *const AtomicU8) {
    // Unlink this static's node (the innermost one, as initializers nest).
    let mut link: *mut *mut Initializing = ptr::null_mut();
    let mut n = initializing_head();
    while !n.is_null() {
        // SAFETY: nodes of this thread's list.
        unsafe {
            if (*n).state == state as usize {
                if link.is_null() {
                    set_initializing_head((*n).next);
                } else {
                    *link = (*n).next;
                }
                libc::free(n.cast());
                break;
            }
            link = &raw mut (*n).next;
            n = (*n).next;
        }
    }
    // SAFETY: as above.
    let s = unsafe { &*state };
    s.store(2, Ordering::Release);
    wake(s);
}

#[cfg(test)]
mod tests {
    use super::*;

    // 0 = uninitialized, 1 = running, 2 = ready.
    #[test]
    fn static_init_states_nest() {
        let outer = AtomicU8::new(0);
        let inner = AtomicU8::new(0);
        assert_eq!(rok_static_init_begin(&raw const outer, ptr::null(), 0), 1);
        assert_eq!(outer.load(Ordering::Relaxed), 1);
        // An initializer touching another static initializes it first.
        assert_eq!(rok_static_init_begin(&raw const inner, ptr::null(), 0), 1);
        rok_static_init_end(&raw const inner);
        assert_eq!(inner.load(Ordering::Relaxed), 2);
        rok_static_init_end(&raw const outer);
        assert_eq!(outer.load(Ordering::Relaxed), 2);
        // Ready statics are not re-run, and the per-thread list is empty.
        assert_eq!(rok_static_init_begin(&raw const outer, ptr::null(), 0), 0);
        assert!(initializing_head().is_null());
    }

    #[test]
    fn wait_times_out_and_wakes() {
        rokr_common::tls::thread_init();
        rokr_gc::world::attach_current();
        let w = AtomicI32::new(7);
        // A timed wait on an unchanged word returns after the timeout.
        wait_on(&w, 7, 1_000_000);
        // A wait on a word that differs returns at once.
        wait_on(&w, 0, -1);
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(5));
                w.store(8, Ordering::SeqCst);
                wake(&w);
            });
            while w.load(Ordering::SeqCst) == 7 {
                wait_on(&w, 7, -1);
            }
        });
    }

    #[test]
    fn wake_one_wakes_one() {
        rokr_common::tls::thread_init();
        rokr_gc::world::attach_current();
        let w = AtomicI32::new(0);
        let woke = AtomicI32::new(0);
        let addr = ptr::from_ref(&w) as usize;
        let waiting = || {
            let b = bucket(addr);
            b.lock.lock();
            let mut n = 0;
            // SAFETY: the bucket's own lock around its list.
            unsafe {
                let mut p = *b.head.get();
                while !p.is_null() {
                    n += i32::from((*p).addr == addr);
                    p = (*p).next;
                }
            }
            b.lock.unlock();
            n
        };
        std::thread::scope(|s| {
            for _ in 0..3 {
                s.spawn(|| {
                    rokr_common::tls::thread_init();
                    rokr_gc::world::attach_current();
                    wait_on(&w, 0, 5_000_000_000);
                    woke.fetch_add(1, Ordering::SeqCst);
                });
            }
            while waiting() < 3 {
                std::thread::yield_now();
            }
            wake_waiters(addr, false);
            while woke.load(Ordering::SeqCst) < 1 {
                std::thread::yield_now();
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            assert_eq!(woke.load(Ordering::SeqCst), 1);
            assert_eq!(waiting(), 2);
            wake(&w);
        });
        assert_eq!(woke.load(Ordering::SeqCst), 3);
    }
}
