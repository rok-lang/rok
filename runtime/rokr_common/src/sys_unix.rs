//! OS primitives shared by the runtime crates: POSIX (macOS, Linux)
//! and WASI through wasi-libc.
//!
//! WASI has no reservations: the heap takes its blocks from `malloc`,
//! which owns `memory.grow`. WASI without threads (no `atomics`:
//! `wasm32-wasip2`, the browser) has one thread: locks are no-ops and a wait nobody can end is a deadlock;
//! `wasm32-wasip1-threads` has wasi-libc's pthreads.

// `static` primitives: `const fn new` is the constructor.
#![allow(clippy::new_without_default)]

#[cfg(any(unix, rok_wasi_threads))]
use core::cell::UnsafeCell;

/// The calling thread's `errno`.
pub fn errno() -> i32 {
    // SAFETY: the libc errno accessor has no preconditions.
    #[cfg(target_vendor = "apple")]
    unsafe {
        *libc::__error()
    }
    // SAFETY: as above.
    #[cfg(not(target_vendor = "apple"))]
    unsafe {
        *libc::__errno_location()
    }
}

/// Write `parts` to `fd` back to back with `writev`, finishing short
/// writes and retrying `EINTR`: 0 or `-errno`. At most eight parts.
pub fn write_all(fd: i32, parts: &[&[u8]]) -> i32 {
    let mut iov = [libc::iovec {
        iov_base: core::ptr::null_mut(),
        iov_len: 0,
    }; 8];
    let mut n = 0;
    for p in parts.iter().filter(|p| !p.is_empty()) {
        iov[n] = libc::iovec {
            iov_base: p.as_ptr().cast_mut().cast(),
            iov_len: p.len(),
        };
        n += 1;
    }
    let mut first = 0;
    while first < n {
        // SAFETY: `iov[first..n]` describe live caller buffers.
        let r = unsafe { libc::writev(fd, iov[first..].as_ptr(), (n - first) as libc::c_int) };
        if r < 0 {
            let e = errno();
            if e == libc::EINTR {
                continue;
            }
            return -e;
        }
        let mut done = r as usize;
        while first < n && done >= iov[first].iov_len {
            done -= iov[first].iov_len;
            first += 1;
        }
        if first < n {
            // SAFETY: `done < iov_len`, inside the part.
            iov[first].iov_base = unsafe { iov[first].iov_base.cast::<u8>().add(done) }.cast();
            iov[first].iov_len -= done;
        }
    }
    0
}

/// One `read` of stdin into `dst` (`len` bytes), retrying `EINTR`: the
/// count (0 at the end) or `-errno`.
///
/// # Safety
/// `dst` must be writable for `len` bytes.
pub unsafe fn read_stdin(dst: *mut u8, len: usize) -> isize {
    loop {
        // SAFETY: caller contract.
        let n = unsafe { libc::read(0, dst.cast(), len) };
        if n >= 0 {
            return n;
        }
        let e = errno();
        if e != libc::EINTR {
            return -(e as isize);
        }
    }
}

/// Whether `fd` is a terminal.
pub fn is_terminal(fd: i32) -> bool {
    // SAFETY: plain libc call.
    unsafe { libc::isatty(fd) != 0 }
}

/// Run `f` at `exit`.
pub fn atexit(f: extern "C" fn()) {
    // SAFETY: registers a plain `extern "C"` function.
    unsafe { libc::atexit(f) };
}

/// A mutex usable from a `static`.
#[cfg(any(unix, rok_wasi_threads))]
pub struct Lock(UnsafeCell<libc::pthread_mutex_t>);

// SAFETY: pthread mutexes are designed for shared use.
#[cfg(any(unix, rok_wasi_threads))]
unsafe impl Sync for Lock {}

/// wasi-libc's initializers are all zero (not in the `libc` crate).
#[cfg(unix)]
const MUTEX_INIT: libc::pthread_mutex_t = libc::PTHREAD_MUTEX_INITIALIZER;
#[cfg(unix)]
const COND_INIT: libc::pthread_cond_t = libc::PTHREAD_COND_INITIALIZER;
// SAFETY: all-zero is wasi-libc's static initializer.
#[cfg(rok_wasi_threads)]
const MUTEX_INIT: libc::pthread_mutex_t = unsafe { core::mem::zeroed() };
// SAFETY: as above.
#[cfg(rok_wasi_threads)]
const COND_INIT: libc::pthread_cond_t = unsafe { core::mem::zeroed() };

#[cfg(any(unix, rok_wasi_threads))]
impl Lock {
    pub const fn new() -> Self {
        Self(UnsafeCell::new(MUTEX_INIT))
    }

    pub fn lock(&self) {
        // SAFETY: a statically initialized mutex.
        unsafe { libc::pthread_mutex_lock(self.0.get()) };
    }

    /// Take the lock if it is free.
    pub fn try_lock(&self) -> bool {
        // SAFETY: as above.
        unsafe { libc::pthread_mutex_trylock(self.0.get()) == 0 }
    }

    /// The caller holds the lock.
    pub fn unlock(&self) {
        // SAFETY: caller contract.
        unsafe { libc::pthread_mutex_unlock(self.0.get()) };
    }
}

/// A condition variable usable from a `static` (or a stack slot that
/// does not move while waited on).
#[cfg(any(unix, rok_wasi_threads))]
pub struct Cond(UnsafeCell<libc::pthread_cond_t>);

#[cfg(any(unix, rok_wasi_threads))]
impl Drop for Cond {
    fn drop(&mut self) {
        // SAFETY: nobody waits on a condvar being dropped.
        unsafe { libc::pthread_cond_destroy(self.0.get()) };
    }
}

// SAFETY: pthread condvars are designed for shared use.
#[cfg(any(unix, rok_wasi_threads))]
unsafe impl Sync for Cond {}

#[cfg(any(unix, rok_wasi_threads))]
impl Cond {
    pub const fn new() -> Self {
        Self(UnsafeCell::new(COND_INIT))
    }

    /// Release `lock` (held by the caller), sleep until woken (or
    /// spuriously), take it again.
    pub fn wait(&self, lock: &Lock) {
        // SAFETY: caller holds `lock`.
        unsafe { libc::pthread_cond_wait(self.0.get(), lock.0.get()) };
    }

    /// [`Self::wait`] for at most `ns` nanoseconds.
    pub fn wait_timeout(&self, lock: &Lock, ns: u64) {
        // `pthread_cond_timedwait` takes a CLOCK_REALTIME deadline.
        let at = realtime_ns().saturating_add(ns.min(i64::MAX as u64) as i64);
        // SAFETY: a zeroed `timespec` is valid; `as _` fits each
        // target's field types (musl's `time_t` alias is deprecated).
        let mut ts: libc::timespec = unsafe { core::mem::zeroed() };
        ts.tv_sec = (at / 1_000_000_000) as _;
        ts.tv_nsec = (at % 1_000_000_000) as _;
        // SAFETY: caller holds `lock`; valid timespec.
        unsafe { libc::pthread_cond_timedwait(self.0.get(), lock.0.get(), &ts) };
    }

    pub fn broadcast(&self) {
        // SAFETY: a statically initialized condvar.
        unsafe { libc::pthread_cond_broadcast(self.0.get()) };
    }
}

#[allow(clippy::unnecessary_cast)] // `time_t` / `c_long` widths vary by target
fn clock_ns(clock: libc::clockid_t) -> i64 {
    // SAFETY: a zeroed `timespec` is valid (32-bit time64 ones have
    // padding fields).
    let mut ts: libc::timespec = unsafe { core::mem::zeroed() };
    // SAFETY: valid out-pointer.
    unsafe { libc::clock_gettime(clock, &mut ts) };
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

/// Monotonic nanoseconds, stopped during suspend (spec 3.3.7: macOS
/// `CLOCK_UPTIME_RAW`, whose `CLOCK_MONOTONIC` counts sleep).
pub fn monotonic_ns() -> i64 {
    #[cfg(target_vendor = "apple")]
    let clock = libc::CLOCK_UPTIME_RAW;
    #[cfg(not(target_vendor = "apple"))]
    let clock = libc::CLOCK_MONOTONIC;
    clock_ns(clock)
}

/// Wall clock, nanoseconds since the Unix epoch.
pub fn realtime_ns() -> i64 {
    clock_ns(libc::CLOCK_REALTIME)
}

/// Nanoseconds since boot, counting suspend: Linux `CLOCK_BOOTTIME`,
/// macOS `CLOCK_MONOTONIC` (3.3.7).
pub fn boot_ns() -> i64 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let clock = libc::CLOCK_BOOTTIME;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let clock = libc::CLOCK_MONOTONIC;
    clock_ns(clock)
}

/// CPU time of the calling thread, nanoseconds (WASI: monotonic time,
/// its only thread never waits for another).
pub fn thread_cpu_ns() -> i64 {
    #[cfg(unix)]
    let clock = libc::CLOCK_THREAD_CPUTIME_ID;
    #[cfg(target_os = "wasi")]
    let clock = libc::CLOCK_MONOTONIC;
    clock_ns(clock)
}

/// Reserve `len` bytes of address space, inaccessible: null on failure.
#[cfg(unix)]
pub fn reserve(len: usize) -> *mut u8 {
    // SAFETY: an anonymous PROT_NONE mapping touches nothing.
    let p = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        core::ptr::null_mut()
    } else {
        p.cast()
    }
}

/// Make the page-aligned range `p..p + len` of a reservation readable
/// and writable (zero-filled on first touch).
///
/// # Safety
/// The range must lie inside a [`reserve`]d region.
#[cfg(unix)]
pub unsafe fn commit(p: *mut u8, len: usize) -> bool {
    // SAFETY: caller contract.
    unsafe { libc::mprotect(p.cast(), len, libc::PROT_READ | libc::PROT_WRITE) == 0 }
}

/// Give the committed page-aligned range back to the OS: it stays
/// accessible and reads as fresh zero pages.
///
/// # Safety
/// The range must be committed and hold nothing anyone still uses.
#[cfg(unix)]
pub unsafe fn reset(p: *mut u8, len: usize) -> bool {
    // SAFETY: caller contract; same protection, new zero pages.
    let q = unsafe {
        libc::mmap(
            p.cast(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED,
            -1,
            0,
        )
    };
    q != libc::MAP_FAILED
}

#[cfg(unix)]
fn page_size() -> usize {
    // SAFETY: plain sysconf query.
    usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(4096)
}

/// Pages of their own for `len` bytes aligned to `align` (a power of
/// two), zero-filled on first touch; given back with [`unmap`]. Null on
/// failure.
#[cfg(unix)]
pub fn map(len: usize, align: usize) -> *mut u8 {
    let page = page_size();
    let len = len.next_multiple_of(page);
    let Some(total) = len.checked_add(align.saturating_sub(page)) else {
        return core::ptr::null_mut();
    };
    // SAFETY: a fresh anonymous mapping.
    let p = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            total,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return core::ptr::null_mut();
    }
    let p = p as usize;
    let at = p.next_multiple_of(align);
    let tail = p + total - (at + len);
    // SAFETY: the unaligned head and the tail of our own mapping.
    unsafe {
        if at > p {
            libc::munmap(p as *mut libc::c_void, at - p);
        }
        if tail > 0 {
            libc::munmap((at + len) as *mut libc::c_void, tail);
        }
    }
    at as *mut u8
}

/// # Safety
/// `p..p + len` must be a whole [`map`] result nobody uses any more.
#[cfg(unix)]
pub unsafe fn unmap(p: *mut u8, len: usize) {
    // SAFETY: caller contract.
    unsafe { libc::munmap(p.cast(), len.next_multiple_of(page_size())) };
}

/// No pages to map: a zeroed allocation.
#[cfg(target_os = "wasi")]
pub fn map(len: usize, align: usize) -> *mut u8 {
    let p = alloc_aligned(len, align);
    if !p.is_null() {
        // SAFETY: a fresh block of `len` bytes.
        unsafe { core::ptr::write_bytes(p, 0, len) };
    }
    p
}

/// # Safety
/// `p` must be a [`map`] result nobody uses any more.
#[cfg(target_os = "wasi")]
pub unsafe fn unmap(p: *mut u8, _len: usize) {
    // SAFETY: caller contract.
    unsafe { free_aligned(p) };
}

/// `size` bytes aligned to `align` (a power of two), freed with
/// [`free_aligned`]: null on failure.
pub fn alloc_aligned(size: usize, align: usize) -> *mut u8 {
    if align <= 16 {
        // SAFETY: plain malloc; its alignment is 16 on supported targets.
        return unsafe { libc::malloc(size) }.cast();
    }
    let mut p: *mut libc::c_void = core::ptr::null_mut();
    // SAFETY: valid out-pointer; `align` is a power of two > 16.
    if unsafe { libc::posix_memalign(&mut p, align, size) } != 0 {
        return core::ptr::null_mut();
    }
    p.cast()
}

/// # Safety
/// `p` must be null or from [`alloc_aligned`].
pub unsafe fn free_aligned(p: *mut u8) {
    // SAFETY: caller contract.
    unsafe { libc::free(p.cast()) };
}

/// Start a detached OS thread running `f()` (default stack): false if
/// it could not be created.
#[cfg(any(unix, rok_wasi_threads))]
pub fn spawn_detached(f: extern "C" fn()) -> bool {
    extern "C" fn start(arg: *mut libc::c_void) -> *mut libc::c_void {
        // SAFETY: `arg` is the `extern "C" fn()` passed below.
        let f: extern "C" fn() = unsafe { core::mem::transmute(arg) };
        f();
        core::ptr::null_mut()
    }
    // SAFETY: plain pthread calls.
    unsafe {
        let mut t: libc::pthread_t = core::mem::zeroed();
        if libc::pthread_create(&mut t, core::ptr::null(), start, f as *mut libc::c_void) != 0 {
            return false;
        }
        libc::pthread_detach(t);
    }
    true
}

/// Block the calling thread until the process exits.
#[cfg(unix)]
pub fn park_forever() -> ! {
    loop {
        // SAFETY: plain libc call.
        unsafe { libc::pause() };
    }
}

/// Block the calling thread until the process exits (no `pause`).
#[cfg(rok_wasi_threads)]
pub fn park_forever() -> ! {
    loop {
        sleep_ns(1 << 40);
    }
}

/// One thread: a lock is never contended.
#[cfg(rok_solo)]
pub struct Lock;

// Not `const`, like the pthread one.
#[cfg(rok_solo)]
#[allow(clippy::missing_const_for_fn)]
impl Lock {
    pub const fn new() -> Self {
        Self
    }

    pub fn lock(&self) {}

    pub fn try_lock(&self) -> bool {
        true
    }

    pub fn unlock(&self) {}
}

/// One thread: nobody else can signal, so an untimed wait never ends.
#[cfg(rok_solo)]
pub struct Cond;

#[cfg(rok_solo)]
impl Cond {
    pub const fn new() -> Self {
        Self
    }

    pub fn wait(&self, _lock: &Lock) {
        crate::fatal(b"fatal error: deadlock: the only thread waits for another");
    }

    pub fn wait_timeout(&self, _lock: &Lock, ns: u64) {
        sleep_ns(ns);
    }

    #[allow(clippy::missing_const_for_fn)]
    pub fn broadcast(&self) {}
}

/// Sleep `ns` nanoseconds (`poll_oneoff` on a clock).
#[cfg(target_os = "wasi")]
pub fn sleep_ns(ns: u64) {
    let ns = ns.min(i64::MAX as u64);
    let mut ts = libc::timespec {
        tv_sec: (ns / 1_000_000_000) as _,
        tv_nsec: (ns % 1_000_000_000) as _,
    };
    // SAFETY: `ts` is valid for read and write (remaining time).
    while unsafe { libc::nanosleep(&ts, &mut ts) } != 0 && errno() == libc::EINTR {}
}

/// Pages cannot be returned to the engine: zero the range.
///
/// # Safety
/// The range must be allocated and hold nothing anyone still uses.
#[cfg(target_os = "wasi")]
pub const unsafe fn reset(p: *mut u8, len: usize) -> bool {
    // SAFETY: caller contract.
    unsafe { core::ptr::write_bytes(p, 0, len) };
    true
}

/// No threads.
#[cfg(rok_solo)]
pub const fn spawn_detached(_f: extern "C" fn()) -> bool {
    false
}

/// Only reached by a thread the runtime has halted: on WASI, the only
/// one, which ends the program.
#[cfg(rok_solo)]
pub fn park_forever() -> ! {
    crate::fatal(b"fatal error: deadlock: the only thread waits for another")
}
