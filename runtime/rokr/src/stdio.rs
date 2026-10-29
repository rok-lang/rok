//! The standard streams. Everything rok writes to fd 1 goes through one
//! runtime buffer behind one lock, flushed when full, at a newline on a
//! terminal, by `rok_stdio_flush`, at `exit` and before a stack-overflow
//! abort. Stderr is unbuffered: one `writev` per call, so a line never
//! interleaves with another thread's. Stdin is one shared read buffer.
//!
//! A thread that has to wait (for a lock or a syscall) does so parked
//! (`os::native`), so a collection never waits on it; the lock holder
//! only blocks parked. Results are 0 / a count, or `-errno`.

use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, Ordering};

use rokr_common::object_header::HEADER_SIZE;
use rokr_common::{sys, write_all};

use crate::os::native;
use crate::str::RokStr;

const CAP: usize = 8192;

#[cfg(any(unix, target_family = "wasm"))]
const BAD_FD: i32 = libc::EBADF;
#[cfg(windows)]
const BAD_FD: i32 = rokr_common::win::ERROR_INVALID_HANDLE;

struct Buf {
    lock: sys::Lock,
    data: UnsafeCell<[u8; CAP]>,
    /// stdout: bytes held; stdin: `[pos, end)` of unread bytes.
    pos: UnsafeCell<usize>,
    end: UnsafeCell<usize>,
}

// SAFETY: every field but `lock` is touched only with `lock` held.
unsafe impl Sync for Buf {}

impl Buf {
    const fn new() -> Self {
        Self {
            lock: sys::Lock::new(),
            data: UnsafeCell::new([0; CAP]),
            pos: UnsafeCell::new(0),
            end: UnsafeCell::new(0),
        }
    }

    /// Take the lock, parked if it is contended.
    fn lock(&self) -> Guard<'_> {
        if !self.lock.try_lock() {
            native(|| self.lock.lock());
        }
        Guard(self)
    }

    /// Take the lock without parking (no mutator needed).
    fn lock_raw(&self) -> Guard<'_> {
        self.lock.lock();
        Guard(self)
    }
}

struct Guard<'a>(&'a Buf);

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.0.lock.unlock();
    }
}

impl Guard<'_> {
    #[allow(clippy::mut_from_ref)] // exclusive while the lock is held
    fn parts(&self) -> (&mut [u8; CAP], &mut usize, &mut usize) {
        // SAFETY: the lock is held, so nobody else touches these.
        unsafe {
            (
                &mut *self.0.data.get(),
                &mut *self.0.pos.get(),
                &mut *self.0.end.get(),
            )
        }
    }
}

static OUT: Buf = Buf::new();
static IN: Buf = Buf::new();
/// `flush_at_exit` is registered: set under `OUT`'s lock before the
/// first byte is buffered.
static AT_EXIT: AtomicBool = AtomicBool::new(false);
/// `isatty(1)`: -1 unknown, then 0 / 1.
static TTY: AtomicI32 = AtomicI32::new(-1);

fn tty() -> bool {
    let t = TTY.load(Ordering::Relaxed);
    if t >= 0 {
        return t != 0;
    }
    let t = sys::is_terminal(1);
    TTY.store(i32::from(t), Ordering::Relaxed);
    t
}

/// Write out the held bytes (lock held). They are dropped on failure,
/// so one bad write does not repeat.
fn drain(held: &[u8], len: &mut usize, parked: bool) -> i32 {
    if *len == 0 {
        return 0;
    }
    let r = if parked {
        native(|| write_all(1, &[&held[..*len]])).0
    } else {
        write_all(1, &[&held[..*len]])
    };
    *len = 0;
    r
}

extern "C" fn flush_at_exit() {
    let g = OUT.lock_raw();
    let (data, len, _) = g.parts();
    drain(data, len, false);
}

/// `parts` to stdout through the buffer.
fn out(parts: &[&[u8]]) -> i32 {
    let g = OUT.lock();
    if !AT_EXIT.load(Ordering::Relaxed) {
        sys::atexit(flush_at_exit);
        AT_EXIT.store(true, Ordering::Relaxed);
    }
    let (data, len, _) = g.parts();
    let mut r = 0;
    let mut newline = false;
    for p in parts {
        if p.len() > CAP - *len {
            r = r.min(drain(data, len, true));
        }
        if p.len() >= CAP {
            r = r.min(native(|| write_all(1, &[p])).0);
        } else {
            data[*len..*len + p.len()].copy_from_slice(p);
            *len += p.len();
            newline |= p.contains(&b'\n');
        }
    }
    if newline && tty() {
        r = r.min(drain(data, len, true));
    }
    r
}

/// `parts` to fd 1 or 2.
pub fn write(fd: i32, parts: &[&[u8]]) -> i32 {
    match fd {
        1 => out(parts),
        2 => native(|| write_all(2, parts)).0,
        _ => -BAD_FD,
    }
}

/// Flush stdout from the stack-overflow handler: only when no thread
/// holds the lock (waiting could deadlock), never parked.
#[cfg(not(target_family = "wasm"))]
pub fn flush_in_signal() {
    // Left locked: the process is about to abort.
    if OUT.lock.try_lock() {
        // SAFETY: the lock is held.
        let (data, len) = unsafe { (&*OUT.data.get(), *OUT.pos.get()) };
        write_all(1, &[&data[..len]]);
    }
}

/// `s` (then `\n` if `newline != 0`) to fd 1 or 2: 0 or `-errno`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_stdio_write(fd: i32, s: RokStr, newline: i32) -> i32 {
    let nl: &[u8] = if newline != 0 { b"\n" } else { b"" };
    write(fd, &[s.bytes(), nl])
}

/// `len` bytes of the `GcBuf<u8>` `buf` from `off` to fd 1 or 2.
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_stdio_write_buf(fd: i32, buf: *mut u8, off: usize, len: usize) -> i32 {
    // SAFETY: caller contract; the caller roots `buf`, so it stays put.
    write(
        fd,
        &[unsafe { core::slice::from_raw_parts(buf.add(HEADER_SIZE + off), len) }],
    )
}

/// Write out the stdout buffer: 0 or `-errno`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_stdio_flush() -> i32 {
    let g = OUT.lock();
    let (data, len, _) = g.parts();
    drain(data, len, true)
}

/// Flush stdout at each newline, as on a terminal: the REPL session
/// host's cells show their output as they write it.
#[unsafe(no_mangle)]
pub extern "C" fn rok_stdio_line_buffered() {
    TTY.store(1, Ordering::Relaxed);
}

/// What reads stdin in place of fd 0 (null: none).
static IN_HOOK: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Read stdin through `hook` (null: fd 0 again). The REPL session host
/// asks rokc for a cell's input this way. The hook fills `len` bytes at
/// most and returns the count (0 at the end of input) or `-errno`;
/// stdout is flushed before each call.
#[unsafe(no_mangle)]
pub extern "C" fn rok_set_stdin_hook(hook: Option<extern "C" fn(*mut u8, usize) -> isize>) {
    IN_HOOK.store(
        hook.map_or(ptr::null_mut(), |h| h as *mut ()),
        Ordering::Release,
    );
}

/// One read of stdin, parked.
pub fn read0(dst: *mut u8, len: usize) -> isize {
    let h = IN_HOOK.load(Ordering::Acquire);
    if h.is_null() {
        // SAFETY: caller passes a writable region of `len` bytes.
        return native(|| unsafe { sys::read_stdin(dst, len) }).0;
    }
    // SAFETY: only `rok_set_stdin_hook` stores, a function of this type.
    let hook =
        unsafe { core::mem::transmute::<*mut (), extern "C" fn(*mut u8, usize) -> isize>(h) };
    rok_stdio_flush();
    native(|| hook(dst, len)).0
}

/// Up to `len` bytes of stdin into the `GcBuf<u8>` `buf` at `off`,
/// stopping after the first `delim` byte (`delim < 0`: no stop). The
/// count (0 at the end of input) or `-errno`.
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_stdin_read(buf: *mut u8, off: usize, len: usize, delim: i32) -> isize {
    // SAFETY: caller contract.
    let dst = unsafe { buf.add(HEADER_SIZE + off) };
    if len == 0 {
        return 0;
    }
    let g = IN.lock();
    let (data, pos, end) = g.parts();
    if *pos == *end {
        if delim < 0 && len >= CAP {
            return read0(dst, len);
        }
        let n = read0(data.as_mut_ptr(), CAP);
        if n <= 0 {
            return n;
        }
        (*pos, *end) = (0, n as usize);
    }
    let avail = &data[*pos..*end];
    let mut n = avail.len().min(len);
    if let Some(i) = avail[..n].iter().position(|&b| i32::from(b) == delim) {
        n = i + 1;
    }
    // SAFETY: `dst` has room for `len >= n` bytes; distinct objects.
    unsafe { ptr::copy_nonoverlapping(avail.as_ptr(), dst, n) };
    *pos += n;
    n as isize
}
