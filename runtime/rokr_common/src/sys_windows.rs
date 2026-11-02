//! OS primitives shared by the runtime crates: Windows (`x86_64` /
//! `aarch64` `pc-windows-msvc`, Windows 10 1803+).
//!
//! fds 0 / 1 / 2 stand for the standard handles; any other "fd" the
//! runtime hands out is a kernel handle value (a multiple of 4, so the
//! two never collide; handles fit in 32 bits by design), or a socket
//! with bit 0 set.
//!
//! Text crossing the API is WTF-8 on the rok side (UTF-8, plus
//! encoded lone surrogates) and UTF-16 on the Windows side.

// `static` primitives: `const fn new` is the constructor.
#![allow(clippy::new_without_default)]

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};

use crate::win::{self, BOOL, DWORD, HANDLE};
pub use crate::wtf8::*;

/// The calling thread's last Win32 error.
pub fn errno() -> i32 {
    // SAFETY: no preconditions.
    unsafe { win::GetLastError() as i32 }
}

/// The handle behind runtime fd `fd`.
pub fn handle(fd: i32) -> HANDLE {
    let std = match fd {
        0 => win::STD_INPUT_HANDLE,
        1 => win::STD_OUTPUT_HANDLE,
        2 => win::STD_ERROR_HANDLE,
        _ => return fd as HANDLE,
    };
    // SAFETY: no preconditions.
    unsafe { win::GetStdHandle(std) }
}

/// Whether `h` is a console (text must go through `*ConsoleW`).
pub fn is_console(h: HANDLE) -> bool {
    let mut mode = 0;
    // SAFETY: valid out-pointer; any handle value is accepted.
    unsafe { win::GetConsoleMode(h, &mut mode) != 0 }
}

/// Whether `fd` is a terminal.
pub fn is_terminal(fd: i32) -> bool {
    is_console(handle(fd))
}

/// `WriteFile` all of `bytes` to `h`: 0 or `-error`.
pub fn write_handle(h: HANDLE, mut bytes: &[u8]) -> i32 {
    while !bytes.is_empty() {
        let n = bytes.len().min(1 << 30) as DWORD;
        let mut done = 0;
        // SAFETY: `bytes` is readable for `n` bytes.
        if unsafe { win::WriteFile(h, bytes.as_ptr().cast(), n, &mut done, ptr::null_mut()) } == 0 {
            return -errno();
        }
        bytes = &bytes[done as usize..];
    }
    0
}

/// The incomplete UTF-8 sequence the last console write to fd 1 / 2
/// ended with, finished by the next one: its length, then its bytes.
static CARRY: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

fn carry(to: &AtomicU32, tail: &[u8]) {
    let mut v = [0u8; 4];
    v[0] = tail.len() as u8;
    v[1..=tail.len()].copy_from_slice(tail);
    to.store(u32::from_le_bytes(v), Ordering::Relaxed);
}

/// UTF-8 `bytes` to the console `h` as UTF-16 (invalid sequences show
/// as U+FFFD). With `held`, a character split across two writes is
/// written whole by the second.
fn write_console(h: HANDLE, mut bytes: &[u8], held: Option<&AtomicU32>) -> i32 {
    let mut buf = [0u16; 1024];
    let mut n = 0;
    if let Some(held) = held {
        let was = held.swap(0, Ordering::Relaxed).to_le_bytes();
        let k = usize::from(was[0]);
        if k > 0 {
            let mut head = [0u8; 7];
            let more = bytes.len().min(4);
            head[..k].copy_from_slice(&was[1..=k]);
            head[k..k + more].copy_from_slice(&bytes[..more]);
            let head = &head[..k + more];
            if incomplete_tail(head) == head.len() {
                carry(held, head);
                return 0;
            }
            let mut d = Decoder { s: head, i: 0 };
            n = d.next().map_or(0, |c| encode_utf16(c, &mut buf));
            bytes = &bytes[d.i - k..];
        }
        let cut = bytes.len() - incomplete_tail(bytes);
        carry(held, &bytes[cut..]);
        bytes = &bytes[..cut];
    }
    let mut d = Decoder { s: bytes, i: 0 };
    loop {
        while n + 2 <= buf.len() {
            let Some(c) = d.next() else { break };
            n += encode_utf16(c, &mut buf[n..]);
        }
        if n == 0 {
            return 0;
        }
        let mut at = 0;
        while at < n {
            let mut done = 0;
            // SAFETY: `buf[at..n]` is initialized.
            let ok = unsafe {
                win::WriteConsoleW(
                    h,
                    buf[at..].as_ptr(),
                    (n - at) as DWORD,
                    &mut done,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                return -errno();
            }
            at += done as usize;
        }
        n = 0;
    }
}

/// Write `parts` to `fd` back to back: 0 or `-error`.
pub fn write_all(fd: i32, parts: &[&[u8]]) -> i32 {
    let h = handle(fd);
    let console = is_console(h);
    let held = match fd {
        1 | 2 => Some(&CARRY[fd as usize - 1]),
        _ => None,
    };
    for p in parts {
        let r = if console {
            write_console(h, p, held)
        } else {
            write_handle(h, p)
        };
        if r < 0 {
            return r;
        }
    }
    0
}

/// One read of stdin into `dst` (`len` bytes): the count (0 at the
/// end) or `-error`. A console reads UTF-16 and hands out UTF-8; a
/// line starting with Ctrl-Z ends the input.
///
/// # Safety
/// `dst` must be writable for `len` bytes.
pub unsafe fn read_stdin(dst: *mut u8, len: usize) -> isize {
    let h = handle(0);
    if !is_console(h) {
        let mut n = 0;
        let want = len.min(1 << 30) as DWORD;
        // SAFETY: caller contract.
        if unsafe { win::ReadFile(h, dst.cast(), want, &mut n, ptr::null_mut()) } == 0 {
            let e = errno();
            return if e == win::ERROR_BROKEN_PIPE {
                0
            } else {
                -(e as isize)
            };
        }
        return n as isize;
    }
    // Each UTF-16 unit becomes at most 3 bytes.
    let mut wide = [0u16; 2048];
    let want = (len / 3).clamp(1, wide.len());
    let mut n = 0;
    // SAFETY: `wide` is writable for `want` units.
    let ok =
        unsafe { win::ReadConsoleW(h, wide.as_mut_ptr(), want as DWORD, &mut n, ptr::null_mut()) };
    if ok == 0 {
        return -(errno() as isize);
    }
    let wide = &wide[..n as usize];
    if wide.first() == Some(&0x1A) {
        return 0;
    }
    let mut out = 0;
    for c in (Utf16 { s: wide, i: 0 }) {
        let mut b = [0u8; 4];
        let k = encode_wtf8(c, &mut b);
        if out + k > len {
            break;
        }
        // SAFETY: `out + k <= len`.
        unsafe { ptr::copy_nonoverlapping(b.as_ptr(), dst.add(out), k) };
        out += k;
    }
    out as isize
}

/// Run `f` at `exit`.
pub fn atexit(f: extern "C" fn()) {
    // SAFETY: registers a plain `extern "C"` function with the UCRT.
    unsafe { win::crt_atexit(f) };
}

/// A mutex usable from a `static` (an SRW lock).
pub struct Lock(UnsafeCell<usize>);

// SAFETY: SRW locks are designed for shared use.
unsafe impl Sync for Lock {}

impl Lock {
    pub const fn new() -> Self {
        Self(UnsafeCell::new(0))
    }

    pub fn lock(&self) {
        // SAFETY: an SRWLOCK_INIT lock that never moves (a `static`).
        unsafe { win::AcquireSRWLockExclusive(self.0.get()) };
    }

    /// Take the lock if it is free.
    pub fn try_lock(&self) -> bool {
        // SAFETY: as above.
        unsafe { win::TryAcquireSRWLockExclusive(self.0.get()) != 0 }
    }

    /// The caller holds the lock.
    pub fn unlock(&self) {
        // SAFETY: caller contract.
        unsafe { win::ReleaseSRWLockExclusive(self.0.get()) };
    }
}

/// A condition variable usable from a `static`.
pub struct Cond(UnsafeCell<usize>);

// SAFETY: condition variables are designed for shared use.
unsafe impl Sync for Cond {}

impl Cond {
    pub const fn new() -> Self {
        Self(UnsafeCell::new(0))
    }

    /// Release `lock` (held by the caller), sleep until woken (or
    /// spuriously), take it again.
    pub fn wait(&self, lock: &Lock) {
        // SAFETY: caller holds `lock`.
        unsafe { win::SleepConditionVariableSRW(self.0.get(), lock.0.get(), win::INFINITE, 0) };
    }

    /// [`Self::wait`] for at most `ns` nanoseconds (rounded up to ms).
    pub fn wait_timeout(&self, lock: &Lock, ns: u64) {
        fine_timers();
        let ms = ns.div_ceil(1_000_000).min(u64::from(win::INFINITE - 1)) as DWORD;
        // SAFETY: caller holds `lock`.
        unsafe { win::SleepConditionVariableSRW(self.0.get(), lock.0.get(), ms, 0) };
    }

    pub fn broadcast(&self) {
        // SAFETY: a CONDITION_VARIABLE_INIT condvar that never moves.
        unsafe { win::WakeAllConditionVariable(self.0.get()) };
    }
}

/// Timed waits run on the system tick (15.6 ms by default); 1 ms
/// matches the executor's timer tick. Set once, on the
/// first timed wait.
fn fine_timers() {
    static SET: AtomicBool = AtomicBool::new(false);
    if !SET.swap(true, Ordering::Relaxed) {
        let mut cur = 0;
        // SAFETY: valid out-pointer; 10 000 × 100 ns = 1 ms.
        unsafe { win::NtSetTimerResolution(10_000, 1, &mut cur) };
    }
}

/// `QueryPerformanceFrequency`, read once.
fn qpc_freq() -> i64 {
    static FREQ: AtomicI64 = AtomicI64::new(0);
    let f = FREQ.load(Ordering::Relaxed);
    if f != 0 {
        return f;
    }
    let mut f = 0;
    // SAFETY: valid out-pointer; never fails on Windows XP+.
    unsafe { win::QueryPerformanceFrequency(&mut f) };
    FREQ.store(f, Ordering::Relaxed);
    f
}

/// Monotonic nanoseconds (`QueryPerformanceCounter`).
pub fn monotonic_ns() -> i64 {
    let mut t = 0;
    // SAFETY: valid out-pointer.
    unsafe { win::QueryPerformanceCounter(&mut t) };
    let f = qpc_freq();
    (t / f) * 1_000_000_000 + (t % f) * 1_000_000_000 / f
}

/// 100 ns ticks between 1601-01-01 and 1970-01-01.
pub const FILETIME_UNIX: i64 = 116_444_736_000_000_000;

/// Wall clock, nanoseconds since the Unix epoch.
pub fn realtime_ns() -> i64 {
    let mut ft = win::FILETIME { lo: 0, hi: 0 };
    // SAFETY: valid out-pointer.
    unsafe { win::GetSystemTimePreciseAsFileTime(&mut ft) };
    filetime_ticks(ft).saturating_sub(FILETIME_UNIX) * 100
}

/// A `FILETIME` as 100 ns ticks.
pub const fn filetime_ticks(ft: win::FILETIME) -> i64 {
    ((ft.hi as i64) << 32) | ft.lo as i64
}

/// Time since boot: the `Instant` clock.
pub fn boot_ns() -> i64 {
    monotonic_ns()
}

/// CPU time of the calling thread, nanoseconds.
pub fn thread_cpu_ns() -> i64 {
    let mut t = [win::FILETIME { lo: 0, hi: 0 }; 4];
    let [created, exited, kernel, user] = &mut t;
    // SAFETY: valid out-pointers; the pseudo handle needs no closing.
    unsafe { win::GetThreadTimes(win::GetCurrentThread(), created, exited, kernel, user) };
    (filetime_ticks(t[2]) + filetime_ticks(t[3])) * 100
}

/// Set by `rok_watch_writes`: the heap's own memory is write-watched
/// (`MEM_WRITE_WATCH`), so the REPL host saves only what changed.
static WATCH: AtomicBool = AtomicBool::new(false);

/// Bumped when write-watched pages change without a write (a reset) or
/// a watched block is released (its address may come back as another).
#[unsafe(no_mangle)]
pub static rok_page_epoch: AtomicU64 = AtomicU64::new(0);

/// Before the runtime starts (the REPL host): reservations and large
/// blocks are write-watched from now on.
#[unsafe(no_mangle)]
pub extern "C" fn rok_watch_writes() {
    WATCH.store(true, Ordering::Relaxed);
}

fn watch() -> DWORD {
    if WATCH.load(Ordering::Relaxed) {
        win::MEM_WRITE_WATCH
    } else {
        0
    }
}

/// Reserve `len` bytes of address space, inaccessible: null on failure.
pub fn reserve(len: usize) -> *mut u8 {
    // SAFETY: a fresh reservation; nothing is touched.
    unsafe {
        win::VirtualAlloc(
            ptr::null_mut(),
            len,
            win::MEM_RESERVE | watch(),
            win::PAGE_NOACCESS,
        )
    }
    .cast()
}

/// Commit the page-aligned range `p..p + len` of a reservation, read /
/// write and zero-filled.
///
/// # Safety
/// The range must lie inside a [`reserve`]d region.
pub unsafe fn commit(p: *mut u8, len: usize) -> bool {
    // SAFETY: caller contract.
    !unsafe { win::VirtualAlloc(p.cast(), len, win::MEM_COMMIT, win::PAGE_READWRITE) }.is_null()
}

/// Give the committed page-aligned range back to the OS: it stays
/// accessible and reads as fresh zero pages.
///
/// # Safety
/// The range must be committed and hold nothing anyone still uses.
pub unsafe fn reset(p: *mut u8, len: usize) -> bool {
    rok_page_epoch.fetch_add(1, Ordering::Release);
    // SAFETY: caller contract.
    unsafe { win::VirtualFree(p.cast(), len, win::MEM_DECOMMIT) != 0 && commit(p, len) }
}

/// Pages of their own for `len` bytes aligned to `align` (a power of
/// two), zero-filled; given back with [`unmap`]. Null on failure.
pub fn map(len: usize, align: usize) -> *mut u8 {
    // Allocations start on the 64 KiB allocation granularity.
    if align <= 1 << 16 {
        let kind = win::MEM_RESERVE | win::MEM_COMMIT | watch();
        // SAFETY: a fresh allocation.
        return unsafe { win::VirtualAlloc(ptr::null_mut(), len, kind, win::PAGE_READWRITE) }
            .cast();
    }
    let Some(total) = len.checked_add(align) else {
        return ptr::null_mut();
    };
    let base = reserve(total);
    if base.is_null() {
        return base;
    }
    let at = base.wrapping_add(base.align_offset(align));
    // SAFETY: `at..at + len` lies inside the fresh reservation.
    if unsafe { commit(at, len) } {
        return at;
    }
    // SAFETY: our own reservation, released whole.
    unsafe { win::VirtualFree(base.cast(), 0, win::MEM_RELEASE) };
    ptr::null_mut()
}

/// # Safety
/// `p` must be a [`map`] result nobody uses any more.
pub unsafe fn unmap(p: *mut u8, _len: usize) {
    // SAFETY: a zeroed out-buffer; `p` lies in its allocation.
    let mut m: win::MEMORY_BASIC_INFORMATION = unsafe { core::mem::zeroed() };
    // SAFETY: as above.
    unsafe { win::VirtualQuery(p.cast(), &mut m, size_of::<win::MEMORY_BASIC_INFORMATION>()) };
    rok_page_epoch.fetch_add(1, Ordering::Release);
    // SAFETY: caller contract; the whole allocation `p` came from.
    unsafe { win::VirtualFree(m.AllocationBase, 0, win::MEM_RELEASE) };
}

/// Blocks from this size on are their own write-watched allocations
/// while writes are watched.
const WATCHED_BLOCK: usize = 1 << 20;

/// `size` bytes aligned to `align` (a power of two), freed with
/// [`free_aligned`]: null on failure.
pub fn alloc_aligned(size: usize, align: usize) -> *mut u8 {
    if watch() != 0 && size >= WATCHED_BLOCK && align <= 1 << 16 {
        let kind = win::MEM_RESERVE | win::MEM_COMMIT | win::MEM_WRITE_WATCH;
        // SAFETY: a fresh allocation (64 KiB-aligned).
        return unsafe { win::VirtualAlloc(ptr::null_mut(), size, kind, win::PAGE_READWRITE) }
            .cast();
    }
    // SAFETY: plain UCRT call.
    unsafe { libc::aligned_malloc(size, align.max(16)) }.cast()
}

/// # Safety
/// `p` must be null or from [`alloc_aligned`].
pub unsafe fn free_aligned(p: *mut u8) {
    // A block of its own starts its allocation; a UCRT block never does
    // (its heap header comes first).
    // SAFETY: a zeroed out-buffer; any address may be queried.
    let mut m: win::MEMORY_BASIC_INFORMATION = unsafe { core::mem::zeroed() };
    let own = !p.is_null()
        && (p as usize).trailing_zeros() >= 16
        // SAFETY: as above.
        && unsafe { win::VirtualQuery(p.cast(), &mut m, size_of::<win::MEMORY_BASIC_INFORMATION>()) } != 0
        && m.AllocationBase == p.cast();
    if own {
        rok_page_epoch.fetch_add(1, Ordering::Release);
        // SAFETY: our own allocation, released whole.
        unsafe { win::VirtualFree(p.cast(), 0, win::MEM_RELEASE) };
        return;
    }
    // SAFETY: caller contract.
    unsafe { libc::aligned_free(p.cast()) };
}

/// Start a detached OS thread running `f()` (default stack): false if
/// it could not be created.
pub fn spawn_detached(f: extern "C" fn()) -> bool {
    unsafe extern "system" fn start(arg: *mut c_void) -> DWORD {
        // SAFETY: `arg` is the `extern "C" fn()` passed below.
        let f: extern "C" fn() = unsafe { core::mem::transmute(arg) };
        f();
        0
    }
    // SAFETY: plain thread creation; the handle is not needed.
    unsafe {
        let h = win::CreateThread(
            ptr::null_mut(),
            0,
            start,
            f as *mut c_void,
            0,
            ptr::null_mut(),
        );
        if h == 0 {
            return false;
        }
        win::CloseHandle(h);
    }
    true
}

/// Block the calling thread until the process exits.
pub fn park_forever() -> ! {
    loop {
        // SAFETY: plain sleep.
        unsafe { win::Sleep(win::INFINITE) };
    }
}

/// `BOOL` as `bool`.
pub const fn ok(b: BOOL) -> bool {
    b != 0
}

/// Call `f(buf, cap)` (a Win32 "fill this buffer" query returning the
/// length written, or the size needed when `cap` is too small, or 0 on
/// failure) until the result fits: the units, or `-error`.
pub fn fill_wide(mut f: impl FnMut(*mut u16, DWORD) -> DWORD) -> Result<Wide, i32> {
    let mut w = Wide::new();
    let mut cap = 512usize;
    loop {
        w.reserve(cap);
        // SAFETY: plain call; `f` may leave the error unset on success.
        unsafe { win::SetLastError(0) };
        // `w` has `cap` units of room.
        let n = f(w.as_mut_ptr(), cap as DWORD) as usize;
        if n == 0 {
            let e = errno();
            if e != 0 {
                return Err(e);
            }
        }
        if n < cap {
            // SAFETY: `f` wrote `n` units.
            unsafe { w.set_len(n) };
            return Ok(w);
        }
        cap = n.max(cap * 2);
    }
}
