//! POSIX helpers behind `rok::std` (stdlib 300: fs, io, env, process).
//! No ABI stability (805 §8.5.12 appendix). WASI through wasi-libc:
//! what preview 1 lacks (processes, signals, `mmap`, locks, `dup`)
//! fails with `-ENOSYS`.
//!
//! - `str` arguments reach libc through [`cs`] (a slice need not be
//!   NUL-terminated).
//! - Results are `>= 0` or `-errno`; resolver failures use codes from
//!   [`EAI_BASE`] up.
//! - Byte data is a region `(buf, off, len)` of the caller's
//!   `GcBuf<u8>`. Variable-length results are copied in up to `len`
//!   bytes and return their full length, so the caller can retry.
//! - Blocking calls are bracketed by `lane::block_begin` / `block_end`
//!   (804 §8.4.10, 706 §7.6.7) and allocate no GC memory meanwhile; the
//!   caller roots every GC argument.

use core::mem::size_of;
use core::ptr;

pub use rokr_common::errno;
use rokr_common::object_header::HEADER_SIZE;

use crate::str::{RokStr, build};

type CStrPtr = *const libc::c_char;

#[cfg(unix)]
const PATH_MAX: usize = libc::PATH_MAX as usize;
#[cfg(target_os = "wasi")]
const PATH_MAX: usize = 4096;

#[cfg(target_os = "wasi")]
const UNSUPPORTED: i32 = -libc::ENOSYS;

/// `r` (a libc `-1`-on-error result) as `>= 0` or `-errno`.
pub fn ret(r: isize) -> isize {
    if r < 0 { -(errno() as isize) } else { r }
}

/// Run a blocking call parked; `errno` survives the re-entry.
pub fn native<R>(f: impl FnOnce() -> R) -> (R, i32) {
    crate::lane::block_begin();
    let r = f();
    let e = errno();
    crate::lane::block_end();
    (r, e)
}

/// `s` as a C string for libc: its own bytes when a NUL follows them,
/// else a `malloc`ed copy (no GC allocation, so it works while parked)
/// freed on drop. Keep it alive across the call: `cs(s).p()` inside the
/// call expression.
pub fn cs(s: RokStr) -> CArg {
    if s.len == 0 {
        return CArg(c"".as_ptr(), false);
    }
    // SAFETY: byte `len` of a `str` is readable (its source's next byte
    // or the NUL after it).
    if unsafe { *s.ptr.add(s.len) } == 0 {
        return CArg(s.ptr.cast(), false);
    }
    // SAFETY: malloc'd `len + 1` bytes, filled and terminated.
    unsafe {
        let p = libc::malloc(s.len + 1).cast::<u8>();
        if p.is_null() {
            rokr_common::oom();
        }
        ptr::copy_nonoverlapping(s.ptr, p, s.len);
        *p.add(s.len) = 0;
        CArg(p.cast_const().cast(), true)
    }
}

/// A NUL-terminated string from [`cs`]; `.1`: owned (malloc'd).
pub struct CArg(CStrPtr, bool);

impl CArg {
    pub const fn p(&self) -> CStrPtr {
        self.0
    }
}

impl Drop for CArg {
    fn drop(&mut self) {
        if self.1 {
            // SAFETY: our own malloc block.
            unsafe { libc::free(self.0.cast_mut().cast()) };
        }
    }
}

/// Element `i` of the `GcBuf<i32>` `buf` (header pointer).
///
/// # Safety
/// `buf` must be a live `GcBuf<i32>` with more than `i` elements.
#[allow(clippy::cast_ptr_alignment)] // GC payloads are 8-aligned
const unsafe fn cell(buf: *mut u8, i: usize) -> *mut i32 {
    // SAFETY: caller contract; elements follow the header.
    unsafe { buf.add(HEADER_SIZE).cast::<i32>().add(i) }
}

/// The writable region `off..off + len` of the `GcBuf<u8>` `buf`.
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + len` bytes.
pub const unsafe fn region(buf: *mut u8, off: usize) -> *mut u8 {
    // SAFETY: caller contract.
    unsafe { buf.add(HEADER_SIZE + off) }
}

/// Copy up to `len` bytes of `src` to `buf[off..]`; `src`'s full length.
fn put(buf: *mut u8, off: usize, len: usize, src: &[u8]) -> isize {
    // SAFETY: the library passes a live region of `len` bytes.
    unsafe { ptr::copy_nonoverlapping(src.as_ptr(), region(buf, off), src.len().min(len)) };
    src.len() as isize
}

/// Bytes of the NUL-terminated `buf` before its NUL.
fn until_nul(buf: &[u8]) -> &[u8] {
    &buf[..buf.iter().position(|&b| b == 0).unwrap_or(buf.len())]
}

// Errors

/// First resolver code: `getaddrinfo` failure `e` is `EAI_BASE + |e|`.
pub const EAI_BASE: i32 = 100_000;

/// `IoErrorKind` discriminants (library/std/src/io.rok declares the same).
mod kind {
    pub const OTHER: i32 = 0;
    pub const NOT_FOUND: i32 = 1;
    pub const PERMISSION_DENIED: i32 = 2;
    pub const ALREADY_EXISTS: i32 = 3;
    pub const NOT_A_DIRECTORY: i32 = 4;
    pub const IS_A_DIRECTORY: i32 = 5;
    pub const DIRECTORY_NOT_EMPTY: i32 = 6;
    pub const INVALID_INPUT: i32 = 7;
    pub const BROKEN_PIPE: i32 = 8;
    pub const INTERRUPTED: i32 = 9;
    pub const WOULD_BLOCK: i32 = 10;
    pub const TIMED_OUT: i32 = 11;
    pub const STORAGE_FULL: i32 = 12;
    pub const CROSSES_DEVICES: i32 = 13;
    pub const UNSUPPORTED: i32 = 14;
}

/// The `IoErrorKind` discriminant of error code `code`.
#[unsafe(no_mangle)]
pub const extern "C" fn rok_os_errno_kind(code: i32) -> i32 {
    match code {
        libc::ENOENT => kind::NOT_FOUND,
        // A name that does not resolve; other resolver failures are Other.
        c if c == EAI_BASE + libc::EAI_NONAME.abs() => kind::NOT_FOUND,
        libc::EACCES | libc::EPERM => kind::PERMISSION_DENIED,
        libc::EEXIST => kind::ALREADY_EXISTS,
        libc::ENOTDIR => kind::NOT_A_DIRECTORY,
        libc::EISDIR => kind::IS_A_DIRECTORY,
        libc::ENOTEMPTY => kind::DIRECTORY_NOT_EMPTY,
        libc::EINVAL => kind::INVALID_INPUT,
        libc::EPIPE => kind::BROKEN_PIPE,
        libc::EINTR => kind::INTERRUPTED,
        libc::EAGAIN => kind::WOULD_BLOCK,
        libc::ETIMEDOUT => kind::TIMED_OUT,
        libc::ENOSPC => kind::STORAGE_FULL,
        libc::EXDEV => kind::CROSSES_DEVICES,
        libc::ENOSYS | libc::ENOTSUP => kind::UNSUPPORTED,
        _ => kind::OTHER,
    }
}

/// The message of error code `code` (`strerror` / `gai_strerror`).
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_strerror(code: i32) -> RokStr {
    #[cfg(any(unix, target_env = "p2"))]
    if code >= EAI_BASE {
        // glibc and wasi-libc number EAI codes negative, BSD positive.
        #[cfg(any(target_os = "linux", target_os = "wasi"))]
        let c = EAI_BASE - code;
        #[cfg(not(any(target_os = "linux", target_os = "wasi")))]
        let c = code - EAI_BASE;
        // SAFETY: gai_strerror returns a static C string.
        let s = unsafe { core::ffi::CStr::from_ptr(libc::gai_strerror(c)) };
        return build(&[s.to_bytes()]);
    }
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is writable for its length.
    unsafe { libc::strerror_r(code, buf.as_mut_ptr().cast(), buf.len()) };
    build(&[until_nul(&buf)])
}

// File descriptors

/// `AT_FDCWD` for a negative `dir` (the root / cwd capability).
const fn at(dir: i32) -> i32 {
    if dir < 0 { libc::AT_FDCWD } else { dir }
}

/// `openat`. `flags` bits: 1 read, 2 write, 4 append, 8 truncate,
/// 16 create, 32 `create_new`, 64 directory. Always `O_CLOEXEC`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_open(dir: i32, path: RokStr, flags: i32, mode: u32) -> i32 {
    let mut o = match (flags & 1 != 0, flags & 6 != 0) {
        (true, true) => libc::O_RDWR,
        (false, true) => libc::O_WRONLY,
        _ => libc::O_RDONLY,
    };
    for (bit, f) in [
        (4, libc::O_APPEND),
        (8, libc::O_TRUNC),
        (16, libc::O_CREAT),
        (32, libc::O_CREAT | libc::O_EXCL),
        (64, libc::O_DIRECTORY),
    ] {
        if flags & bit != 0 {
            o |= f;
        }
    }
    // SAFETY: `path` is a C string; the mode is read only with O_CREAT.
    let (r, _) = native(|| unsafe {
        libc::openat(
            at(dir),
            cs(path).p(),
            o | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    });
    ret(r as isize) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_os_close(fd: i32) -> i32 {
    // SAFETY: plain libc call.
    ret(unsafe { libc::close(fd) } as isize) as i32
}

/// One `read` into `buf[off..off + len]`: the count (0 = EOF) or `-errno`.
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_os_read(fd: i32, buf: *mut u8, off: usize, len: usize) -> isize {
    // SAFETY: caller contract.
    let dst = unsafe { region(buf, off) };
    if fd == 0 {
        return crate::stdio::read0(dst, len);
    }
    native(|| {
        loop {
            // SAFETY: `dst` is writable for `len` bytes.
            let n = ret(unsafe { libc::read(fd, dst.cast(), len) });
            if n != -(libc::EINTR as isize) {
                break n;
            }
        }
    })
    .0
}

/// One `write` of `buf[off..off + len]`: the count or `-errno`.
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_os_write(fd: i32, buf: *mut u8, off: usize, len: usize) -> isize {
    // SAFETY: caller contract.
    let src = unsafe { region(buf, off) };
    native(|| {
        loop {
            // SAFETY: `src` is readable for `len` bytes.
            let n = ret(unsafe { libc::write(fd, src.cast(), len) });
            if n != -(libc::EINTR as isize) {
                break n;
            }
        }
    })
    .0
}

/// Wait until `fd1` or `fd2` (negative: ignored) is readable or hung
/// up: bit 0 / bit 1 set for each ready one, or `-errno`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_poll_read(fd1: i32, fd2: i32) -> i32 {
    let mut fds = [fd1, fd2].map(|fd| libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    });
    native(|| {
        loop {
            // SAFETY: two entries; negative fds are ignored.
            if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } >= 0 {
                break i32::from(fds[0].revents != 0) | (i32::from(fds[1].revents != 0) << 1);
            }
            let e = errno();
            if e != libc::EINTR {
                break -e;
            }
        }
    })
    .0
}

/// `fsync` (`data_only == 0`) or `fdatasync`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_fsync(fd: i32, data_only: i32) -> i32 {
    // SAFETY: plain libc calls.
    let (r, e) = native(|| unsafe {
        #[cfg(not(target_vendor = "apple"))]
        if data_only != 0 {
            return libc::fdatasync(fd);
        }
        let _ = data_only;
        libc::fsync(fd)
    });
    if r < 0 { -e } else { 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_os_ftruncate(fd: i32, len: u64) -> i32 {
    // SAFETY: plain libc call.
    ret(unsafe { libc::ftruncate(fd, len as libc::off_t) } as isize) as i32
}

/// `lseek`; `whence` 0 start, 1 current, 2 end. New offset or `-errno`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_lseek(fd: i32, off: i64, whence: i32) -> i64 {
    let w = match whence {
        1 => libc::SEEK_CUR,
        2 => libc::SEEK_END,
        _ => libc::SEEK_SET,
    };
    // SAFETY: plain libc call.
    let r = unsafe { libc::lseek(fd, off as libc::off_t, w) };
    if r < 0 { -i64::from(errno()) } else { r as i64 }
}

#[cfg(unix)]
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_dup(fd: i32) -> i32 {
    // SAFETY: plain libc call.
    ret(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) } as isize) as i32
}

#[cfg(target_os = "wasi")]
#[unsafe(no_mangle)]
pub const extern "C" fn rok_os_dup(_fd: i32) -> i32 {
    UNSUPPORTED
}

#[cfg(unix)]
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_fchmod(fd: i32, mode: u32) -> i32 {
    // SAFETY: plain libc call.
    ret(unsafe { libc::fchmod(fd, mode as libc::mode_t) } as isize) as i32
}

/// WASI files have no permission bits (wasi-libc makes `st_mode` up):
/// nothing to set.
#[cfg(target_os = "wasi")]
#[unsafe(no_mangle)]
pub const extern "C" fn rok_os_fchmod(_fd: i32, _mode: u32) -> i32 {
    0
}

/// A read-only shared mapping of `len` bytes of `fd` at `offset`: its
/// address, or `-errno`.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_mmap(fd: i32, len: usize, offset: u64) -> i64 {
    if len == 0 {
        return -i64::from(libc::EINVAL);
    }
    // SAFETY: a fresh mapping; the kernel picks the address.
    let (p, e) = native(|| unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            offset as libc::off_t,
        )
    });
    if p == libc::MAP_FAILED {
        -i64::from(e)
    } else {
        p as i64
    }
}

#[cfg(target_os = "wasi")]
#[unsafe(no_mangle)]
pub const extern "C" fn rok_os_mmap(_fd: i32, _len: usize, _offset: u64) -> i64 {
    UNSUPPORTED as i64
}

/// Unmap a `rok_os_mmap` region: 0 or `-errno`.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_munmap(addr: usize, len: usize) -> i32 {
    // SAFETY: `addr..addr + len` is a mapping from `rok_os_mmap`.
    ret(unsafe { libc::munmap(addr as *mut libc::c_void, len) } as isize) as i32
}

#[cfg(target_os = "wasi")]
#[unsafe(no_mangle)]
pub const extern "C" fn rok_os_munmap(_addr: usize, _len: usize) -> i32 {
    UNSUPPORTED
}

/// Copy `len` bytes at `addr + at` into `buf[off..]`.
///
/// # Safety
/// `addr + at .. + len` must be mapped; `buf` a live `GcBuf<u8>` with at
/// least `off + len` bytes.
#[unsafe(no_mangle)]
pub const unsafe extern "C" fn rok_os_mmap_copy(
    addr: usize,
    at: usize,
    buf: *mut u8,
    off: usize,
    len: usize,
) {
    // SAFETY: caller contract.
    unsafe { core::ptr::copy_nonoverlapping((addr + at) as *const u8, region(buf, off), len) };
}

/// `flock`: `op` 1 shared, 2 exclusive, 8 unlock, | 4 non-blocking.
/// 1 = locked, 0 = would block (non-blocking), `-errno`.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_flock(fd: i32, op: i32) -> i32 {
    let mut o = 0;
    for (bit, f) in [
        (1, libc::LOCK_SH),
        (2, libc::LOCK_EX),
        (4, libc::LOCK_NB),
        (8, libc::LOCK_UN),
    ] {
        if op & bit != 0 {
            o |= f;
        }
    }
    // SAFETY: plain libc call.
    let (r, e) = native(|| unsafe { libc::flock(fd, o) });
    match (r, e) {
        (0, _) => 1,
        (_, libc::EWOULDBLOCK) if op & 4 != 0 => 0,
        _ => -e,
    }
}

#[cfg(target_os = "wasi")]
#[unsafe(no_mangle)]
pub const extern "C" fn rok_os_flock(_fd: i32, _op: i32) -> i32 {
    UNSUPPORTED
}

/// Copy `src` → `dst` from their current offsets to EOF: bytes or `-errno`.
/// The kernel moves the bytes where the pair of descriptor kinds allows
/// ([`kernel_copy`]); every other pair, and one the kernel or file system
/// refuses before a byte moved, takes the read / write loop. Non-blocking
/// ends are waited on with `poll`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_copy_fd(src: i32, dst: i32) -> i64 {
    let (r, _) = native(|| {
        let mut total = 0;
        kernel_copy(src, dst, &mut total).unwrap_or_else(|| rw_copy(src, dst, total))
    });
    r.unwrap_or_else(|e| -i64::from(e))
}

/// What a descriptor is, for picking the copy mechanism.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Clone, Copy, PartialEq, Eq)]
enum FdKind {
    File,
    Pipe,
    Sock,
    Other,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn fd_kind(fd: i32) -> FdKind {
    // SAFETY: zeroed `stat` is a valid out-buffer.
    let mut st: libc::stat = unsafe { core::mem::zeroed() };
    // SAFETY: plain syscall.
    if unsafe { libc::fstat(fd, &raw mut st) } < 0 {
        return FdKind::Other;
    }
    match st.st_mode & libc::S_IFMT {
        libc::S_IFREG => FdKind::File,
        libc::S_IFIFO => FdKind::Pipe,
        libc::S_IFSOCK => FdKind::Sock,
        _ => FdKind::Other,
    }
}

/// Wait until `fd` is ready for `events` (after `EAGAIN`).
fn wait_fd(fd: i32, events: libc::c_short) {
    let mut p = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    // SAFETY: one valid pollfd; an `EINTR` only retries the copy step.
    unsafe { libc::poll(&raw mut p, 1, -1) };
}

/// After `EAGAIN` from a kernel copy: wait on `dst` if it takes no bytes
/// now, else on `src`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_pair(src: i32, dst: i32) {
    let mut p = libc::pollfd {
        fd: dst,
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: one valid pollfd, no wait.
    if unsafe { libc::poll(&raw mut p, 1, 0) } == 1 {
        wait_fd(src, libc::POLLIN);
    } else {
        wait_fd(dst, libc::POLLOUT);
    }
}

/// Errors with which a kernel copy declines a pair (kernel, file system,
/// `O_APPEND`, descriptor type) rather than failing it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn declines(e: i32) -> bool {
    [
        libc::EXDEV,
        libc::ENOSYS,
        libc::EINVAL,
        libc::EOPNOTSUPP,
        libc::ENOTSUP,
        libc::EPERM,
        libc::EBADF,
        libc::ENOTSOCK,
    ]
    .contains(&e)
}

/// Repeat the kernel copy `step` (bytes moved, 0 at EOF, or -1 with
/// `errno`) to EOF, counting into `total`; `None` if it declines the
/// pair before a byte moved.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn pump(
    src: i32,
    dst: i32,
    total: &mut i64,
    mut step: impl FnMut() -> isize,
) -> Option<Result<i64, i32>> {
    loop {
        let n = step();
        if n > 0 {
            *total += n as i64;
            continue;
        }
        if n == 0 {
            return Some(Ok(*total));
        }
        match errno() {
            libc::EINTR => {}
            libc::EAGAIN => wait_pair(src, dst),
            e if *total == 0 && declines(e) => return None,
            e => return Some(Err(e)),
        }
    }
}

/// The in-kernel copy for this pair of descriptor kinds (Linux):
/// `copy_file_range` file → file (a reflink or server-side copy where the
/// file system can), `sendfile` file → anything else, `splice` to or from
/// a pipe, and a socket → file / socket through a pipe. `None` where it
/// does not apply.
#[cfg(target_os = "linux")]
fn kernel_copy(src: i32, dst: i32, total: &mut i64) -> Option<Result<i64, i32>> {
    use FdKind::{File, Pipe, Sock};
    const CHUNK: usize = 1 << 30;
    let null = ptr::null_mut::<libc::loff_t>();
    // SAFETY (each step): null offsets use and advance the descriptors' own.
    match (fd_kind(src), fd_kind(dst)) {
        (File, File) => pump(src, dst, total, || unsafe {
            libc::copy_file_range(src, null, dst, null, CHUNK, 0)
        }),
        (File, _) => pump(src, dst, total, || unsafe {
            libc::sendfile(dst, src, ptr::null_mut(), CHUNK)
        }),
        (Pipe, _) | (_, Pipe) => pump(src, dst, total, || unsafe {
            libc::splice(src, null, dst, null, CHUNK, libc::SPLICE_F_MOVE)
        }),
        (Sock, File | Sock) => splice_through_pipe(src, dst, total),
        _ => None,
    }
}

/// The in-kernel copy for this pair (macOS): `sendfile` file → socket,
/// from the file's offset, which then moves past what was sent.
#[cfg(target_os = "macos")]
fn kernel_copy(src: i32, dst: i32, total: &mut i64) -> Option<Result<i64, i32>> {
    if fd_kind(src) != FdKind::File || fd_kind(dst) != FdKind::Sock {
        return None;
    }
    // SAFETY: plain syscall.
    let start = unsafe { libc::lseek(src, 0, libc::SEEK_CUR) };
    if start < 0 {
        return None;
    }
    let mut off = start;
    let r = pump(src, dst, total, || {
        // 0: to the file's end; on return, what was sent (also on `EAGAIN`).
        let mut len: libc::off_t = 0;
        // SAFETY: plain syscall with a valid length pointer.
        let r = unsafe { libc::sendfile(src, dst, off, &raw mut len, ptr::null_mut(), 0) };
        off += len;
        if len > 0 { len as isize } else { r as isize }
    });
    // SAFETY: plain syscall.
    unsafe { libc::lseek(src, start + *total, libc::SEEK_SET) };
    r
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const fn kernel_copy(_src: i32, _dst: i32, _total: &mut i64) -> Option<Result<i64, i32>> {
    None
}

/// Socket → file / socket with `splice(2)` through a pipe of our own.
#[cfg(target_os = "linux")]
fn splice_through_pipe(src: i32, dst: i32, total: &mut i64) -> Option<Result<i64, i32>> {
    let mut p = [0i32; 2];
    // SAFETY: `p` holds two fds.
    if unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return None;
    }
    let null = ptr::null_mut::<libc::loff_t>();
    let r = loop {
        // SAFETY: plain syscall on the caller's socket and our pipe.
        let n = unsafe { libc::splice(src, null, p[1], null, 1 << 16, libc::SPLICE_F_MOVE) };
        if n == 0 {
            break Some(Ok(*total));
        }
        if n < 0 {
            match errno() {
                libc::EINTR => {}
                libc::EAGAIN => wait_fd(src, libc::POLLIN),
                e if *total == 0 && declines(e) => break None,
                e => break Some(Err(e)),
            }
            continue;
        }
        // What entered the pipe must leave it: from here on a refusal is
        // an error, not a reason to fall back.
        let (mut left, mut err) = (n as usize, 0);
        while left > 0 && err == 0 {
            // SAFETY: as above.
            let k = unsafe { libc::splice(p[0], null, dst, null, left, libc::SPLICE_F_MOVE) };
            if k > 0 {
                left -= k as usize;
                continue;
            }
            err = if k == 0 { libc::EIO } else { errno() };
            match err {
                libc::EINTR => err = 0,
                libc::EAGAIN => {
                    wait_fd(dst, libc::POLLOUT);
                    err = 0;
                }
                _ => {}
            }
        }
        if err != 0 {
            break Some(Err(err));
        }
        *total += n as i64;
    };
    // SAFETY: our pipe.
    unsafe {
        libc::close(p[0]);
        libc::close(p[1]);
    }
    r
}

/// The read / write loop: pairs no kernel copy takes.
fn rw_copy(src: i32, dst: i32, mut total: i64) -> Result<i64, i32> {
    let mut buf = [0u8; 16 * 1024];
    loop {
        // SAFETY: `buf` is writable for its length.
        let n = unsafe { libc::read(src, buf.as_mut_ptr().cast(), buf.len()) };
        if n == 0 {
            return Ok(total);
        }
        if n < 0 {
            match errno() {
                libc::EINTR => {}
                libc::EAGAIN => wait_fd(src, libc::POLLIN),
                e => return Err(e),
            }
            continue;
        }
        let mut off = 0;
        while off < n as usize {
            // SAFETY: `buf[off..n]` is initialized.
            let w = unsafe { libc::write(dst, buf.as_ptr().add(off).cast(), n as usize - off) };
            if w < 0 {
                match errno() {
                    libc::EINTR => {}
                    libc::EAGAIN => wait_fd(dst, libc::POLLOUT),
                    e => return Err(e),
                }
                continue;
            }
            off += w as usize;
        }
        total += n as i64;
    }
}

// Paths and directories

/// Reflink `from` (a regular file) to a new `to`, both under `dir`
/// (macOS `clonefileat`: same-volume APFS copies share their blocks).
/// 0, or `-errno` (`EEXIST`, cross-volume, no support) - the caller
/// then copies the bytes. Elsewhere always `-ENOTSUP` (Linux reflinks
/// through `copy_file_range` in `rok_os_copy_fd`).
#[unsafe(no_mangle)]
#[cfg_attr(not(target_os = "macos"), allow(clippy::missing_const_for_fn))]
pub extern "C" fn rok_os_clone(dir: i32, from: RokStr, to: RokStr) -> i32 {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: both are C strings.
        let (r, e) =
            native(|| unsafe { libc::clonefileat(at(dir), cs(from).p(), at(dir), cs(to).p(), 0) });
        if r < 0 { -e } else { 0 }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (dir, from, to);
        -libc::ENOTSUP
    }
}

/// The kind `rok_os_stat` reports for file mode `m`.
// `mode_t` is u16 on macOS, u32 elsewhere.
#[allow(clippy::unnecessary_cast)]
const fn kind_of(m: u32) -> i64 {
    match m & libc::S_IFMT as u32 {
        x if x == libc::S_IFREG as u32 => 1,
        x if x == libc::S_IFDIR as u32 => 2,
        x if x == libc::S_IFLNK as u32 => 3,
        _ => 0,
    }
}

/// `rok_os_stat` through `statx` (Linux 4.11+), the only call that
/// reports creation time; `Err(errno)`.
#[cfg(target_os = "linux")]
fn statx(dir: i32, path: RokStr, flags: i32, fd: i32) -> Result<[i64; 12], i32> {
    // SAFETY: zeroed `statx` is a valid out-buffer.
    let mut sx: libc::statx = unsafe { core::mem::zeroed() };
    let mask = libc::STATX_BASIC_STATS | libc::STATX_BTIME;
    // SAFETY: C strings; `sx` is writable.
    let (r, e) = native(|| unsafe {
        if fd >= 0 {
            let f = flags | libc::AT_EMPTY_PATH;
            libc::syscall(libc::SYS_statx, fd, c"".as_ptr(), f, mask, &raw mut sx)
        } else {
            libc::syscall(
                libc::SYS_statx,
                at(dir),
                cs(path).p(),
                flags,
                mask,
                &raw mut sx,
            )
        }
    });
    if r < 0 {
        return Err(e);
    }
    let ts = |t: libc::statx_timestamp| [t.tv_sec, i64::from(t.tv_nsec)];
    let [mtime, mtime_ns] = ts(sx.stx_mtime);
    let [atime, atime_ns] = ts(sx.stx_atime);
    let [ctime, ctime_ns] = ts(sx.stx_ctime);
    let [btime, btime_ns] = ts(sx.stx_btime);
    let born = i64::from(sx.stx_mask & libc::STATX_BTIME != 0);
    let mode = u32::from(sx.stx_mode);
    Ok([
        kind_of(mode),
        sx.stx_size as i64,
        i64::from(mode & 0o7777),
        mtime,
        mtime_ns,
        atime,
        ctime,
        atime_ns,
        ctime_ns,
        born,
        btime,
        btime_ns,
    ])
}

/// `fstatat` (`fd < 0`) or `fstat(fd)`, filling the `GcBuf<i64>` `out`:
/// `[kind, size, mode, mtime_s, mtime_ns, atime_s, ctime_s, atime_ns,
/// ctime_ns, has birth time, btime_s, btime_ns]`, kind 1 file, 2 dir,
/// 3 symlink, 0 other. 0 or `-errno`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_stat(dir: i32, path: RokStr, follow: i32, fd: i32, out: *mut u8) -> i32 {
    let flags = if follow != 0 {
        0
    } else {
        libc::AT_SYMLINK_NOFOLLOW
    };
    #[cfg(target_os = "linux")]
    match statx(dir, path, flags, fd) {
        Ok(vals) => {
            // SAFETY: the library passes a live `GcBuf<i64>` of 12 elements.
            unsafe { ptr::copy_nonoverlapping(vals.as_ptr(), out.add(HEADER_SIZE).cast(), 12) };
            return 0;
        }
        // No statx (old kernel, or a sandbox that refuses it).
        Err(libc::ENOSYS | libc::EPERM) => {}
        Err(e) => return -e,
    }
    // SAFETY: zeroed `stat` is a valid out-buffer.
    let mut st: libc::stat = unsafe { core::mem::zeroed() };
    // SAFETY: `path` is a C string; `st` is writable.
    let (r, e) = native(|| unsafe {
        if fd >= 0 {
            libc::fstat(fd, &mut st)
        } else {
            libc::fstatat(at(dir), cs(path).p(), &mut st, flags)
        }
    });
    if r < 0 {
        return -e;
    }
    #[allow(clippy::unnecessary_cast)]
    let kind = kind_of(st.st_mode as u32);
    // macOS keeps the creation time in `stat`.
    #[cfg(target_os = "macos")]
    let born = [1, st.st_birthtime, st.st_birthtime_nsec];
    #[cfg(not(target_os = "macos"))]
    let born = [0, 0, 0];
    // The fields are i32 or i64 by target.
    #[cfg(unix)]
    #[allow(clippy::unnecessary_cast)]
    let vals = [
        kind,
        st.st_size as i64,
        i64::from(st.st_mode & 0o7777),
        st.st_mtime as i64,
        st.st_mtime_nsec as i64,
        st.st_atime as i64,
        st.st_ctime as i64,
        st.st_atime_nsec as i64,
        st.st_ctime_nsec as i64,
        born[0],
        born[1],
        born[2],
    ];
    #[cfg(target_os = "wasi")]
    let vals = [
        kind,
        st.st_size as i64,
        i64::from(st.st_mode & 0o7777),
        st.st_mtim.tv_sec,
        st.st_mtim.tv_nsec as i64,
        st.st_atim.tv_sec,
        st.st_ctim.tv_sec,
        st.st_atim.tv_nsec as i64,
        st.st_ctim.tv_nsec as i64,
        born[0],
        born[1],
        born[2],
    ];
    // SAFETY: the library passes a live `GcBuf<i64>` of 12 elements.
    unsafe { ptr::copy_nonoverlapping(vals.as_ptr(), out.add(HEADER_SIZE).cast(), vals.len()) };
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_os_mkdir(dir: i32, path: RokStr, mode: u32) -> i32 {
    // SAFETY: `path` is a C string.
    let (r, e) = native(|| unsafe { libc::mkdirat(at(dir), cs(path).p(), mode as libc::mode_t) });
    if r < 0 { -e } else { 0 }
}

/// `unlinkat`; `is_dir != 0` removes an empty directory.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_unlink(dir: i32, path: RokStr, is_dir: i32) -> i32 {
    let flags = if is_dir != 0 { libc::AT_REMOVEDIR } else { 0 };
    // SAFETY: `path` is a C string.
    let (r, e) = native(|| unsafe { libc::unlinkat(at(dir), cs(path).p(), flags) });
    if r < 0 { -e } else { 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_os_rename(dir: i32, from: RokStr, to_dir: i32, to: RokStr) -> i32 {
    // SAFETY: both are C strings.
    let (r, e) =
        native(|| unsafe { libc::renameat(at(dir), cs(from).p(), at(to_dir), cs(to).p()) });
    if r < 0 { -e } else { 0 }
}

/// `hard == 0`: symlink `link` → `target`; else hard link.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_link(dir: i32, target: RokStr, link: RokStr, hard: i32) -> i32 {
    // SAFETY: both are C strings.
    let (r, e) = native(|| unsafe {
        if hard != 0 {
            libc::linkat(at(dir), cs(target).p(), at(dir), cs(link).p(), 0)
        } else {
            libc::symlinkat(cs(target).p(), at(dir), cs(link).p())
        }
    });
    if r < 0 { -e } else { 0 }
}

/// Open `path` under `dir` for listing: a `DIR*` (its address, zero
/// extended: a 32-bit one may have the top bit set) or `-errno`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_opendir(dir: i32, path: RokStr) -> i64 {
    // SAFETY: `path` is a C string; `fdopendir` owns `fd` on success.
    let (d, e) = native(|| unsafe {
        let fd = libc::openat(
            at(dir),
            cs(path).p(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        );
        if fd < 0 {
            return ptr::null_mut();
        }
        let d = libc::fdopendir(fd);
        if d.is_null() {
            libc::close(fd);
        }
        d
    });
    if d.is_null() {
        -i64::from(e)
    } else {
        d as usize as i64
    }
}

/// The next entry name (skipping `.` / `..`) into `buf[off..off + len]`
/// (room for any name: 1024 bytes): its length, 0 at the end, or `-errno`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_readdir(d: isize, buf: *mut u8, off: usize, len: usize) -> isize {
    loop {
        // SAFETY: errno reset distinguishes end from failure.
        unsafe { set_errno(0) };
        // SAFETY: `d` is a live `DIR*` from `rok_os_opendir`.
        let ent = unsafe { libc::readdir(d as *mut libc::DIR) };
        if ent.is_null() {
            return -(errno() as isize);
        }
        // SAFETY: `d_name` is NUL-terminated.
        let name = unsafe { core::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
        if name.len() > len {
            return -(libc::ENAMETOOLONG as isize);
        }
        if name != b"." && name != b".." {
            return put(buf, off, len, name);
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_os_closedir(d: isize) {
    // SAFETY: `d` is a live `DIR*`.
    unsafe { libc::closedir(d as *mut libc::DIR) };
}

unsafe fn set_errno(v: i32) {
    // SAFETY: errno is this thread's.
    #[cfg(target_vendor = "apple")]
    unsafe {
        *libc::__error() = v;
    }
    // SAFETY: as above.
    #[cfg(not(target_vendor = "apple"))]
    unsafe {
        *libc::__errno_location() = v;
    }
}

/// The working directory into `buf[off..off + len]`: its full length or `-errno`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_getcwd(buf: *mut u8, off: usize, len: usize) -> isize {
    let mut cwd = [0u8; PATH_MAX + 1];
    // SAFETY: `cwd` is writable for its length.
    if unsafe { libc::getcwd(cwd.as_mut_ptr().cast(), cwd.len()) }.is_null() {
        return -(errno() as isize);
    }
    put(buf, off, len, until_nul(&cwd))
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_os_chdir(path: RokStr) -> i32 {
    // SAFETY: `path` is a C string.
    ret(unsafe { libc::chdir(cs(path).p()) } as isize) as i32
}

/// `realpath(path)` into `buf[off..off + len]`: its full length or `-errno`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_realpath(path: RokStr, buf: *mut u8, off: usize, len: usize) -> isize {
    let mut real = [0u8; PATH_MAX + 1];
    // SAFETY: `real` has PATH_MAX bytes.
    let (p, e) = native(|| unsafe { libc::realpath(cs(path).p(), real.as_mut_ptr().cast()) });
    if p.is_null() {
        return -(e as isize);
    }
    put(buf, off, len, until_nul(&real))
}

#[cfg(target_vendor = "apple")]
unsafe extern "C" {
    #[link_name = "_NSGetExecutablePath"]
    fn ns_get_executable_path(buf: *mut libc::c_char, size: *mut u32) -> i32;
}

/// Absolute path of the running executable into `buf[off..off + len]`:
/// its full length or `-errno`.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_current_exe(buf: *mut u8, off: usize, len: usize) -> isize {
    let mut path = [0u8; PATH_MAX + 1];
    #[cfg(target_vendor = "apple")]
    {
        let mut raw = [0u8; PATH_MAX + 1];
        let mut size = raw.len() as u32;
        // SAFETY: `raw` is writable for `size` bytes.
        if unsafe { ns_get_executable_path(raw.as_mut_ptr().cast(), &mut size) } != 0 {
            return -(libc::ENAMETOOLONG as isize);
        }
        // SAFETY: `raw` is NUL-terminated; `path` has PATH_MAX bytes.
        if unsafe { libc::realpath(raw.as_ptr().cast(), path.as_mut_ptr().cast()) }.is_null() {
            return -(errno() as isize);
        }
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        // SAFETY: `path` is writable for its length minus the NUL.
        let n = unsafe {
            libc::readlink(
                c"/proc/self/exe".as_ptr(),
                path.as_mut_ptr().cast(),
                path.len() - 1,
            )
        };
        if n < 0 {
            return -(errno() as isize);
        }
    }
    put(buf, off, len, until_nul(&path))
}

#[cfg(target_os = "wasi")]
#[unsafe(no_mangle)]
pub const extern "C" fn rok_os_current_exe(_buf: *mut u8, _off: usize, _len: usize) -> isize {
    UNSUPPORTED as isize
}

// Environment

/// `environ` (Apple: through `_NSGetEnviron`, as executables only see it
/// there; WASI: wasi-libc's, filled on first use).
fn environ() -> *mut *const CStrPtr {
    #[cfg(target_vendor = "apple")]
    {
        // SAFETY: always valid in a running process.
        unsafe { libc::_NSGetEnviron().cast() }
    }
    #[cfg(all(unix, not(target_vendor = "apple")))]
    {
        unsafe extern "C" {
            static mut environ: *const CStrPtr;
        }
        &raw mut environ
    }
    #[cfg(target_os = "wasi")]
    {
        unsafe extern "C" {
            static mut __wasilibc_environ: *const CStrPtr;
            fn __wasilibc_ensure_environ();
        }
        // SAFETY: fills `__wasilibc_environ` once.
        unsafe { __wasilibc_ensure_environ() };
        &raw mut __wasilibc_environ
    }
}

/// The C `main`'s argc / argv, captured before `main` runs.
#[cfg(all(unix, not(target_vendor = "apple")))]
static mut ARGS: (i32, *const CStrPtr) = (0, ptr::null());

/// Captured before `main` by an `.init_array` entry. glibc passes it
/// `argc, argv, envp`. musl passes nothing: the initial stack, which the
/// kernel lays out as `argc, argv[0..argc], NULL, envp..`, is found
/// through `environ`, still the kernel's `envp` then (glibc's 32-bit Arm
/// `_start` overwrites that `argc` with a pointer).
#[cfg(all(unix, not(target_vendor = "apple")))]
#[used]
#[unsafe(link_section = ".init_array")]
static CAPTURE_ARGS: extern "C" fn(i32, *const CStrPtr) = capture_args;

#[cfg(all(unix, not(target_vendor = "apple"), not(target_env = "musl")))]
extern "C" fn capture_args(argc: i32, argv: *const CStrPtr) {
    // SAFETY: runs once, single-threaded, before `main`.
    unsafe { ARGS = (argc, argv) };
}

#[cfg(target_env = "musl")]
extern "C" fn capture_args(_: i32, _: *const CStrPtr) {
    // SAFETY: runs once, single-threaded, before `main`, while `environ`
    // is still the kernel's `envp`.
    unsafe {
        if let Some(a) = argv_before(*environ()) {
            ARGS = a;
        }
    }
}

/// `(argc, argv)` laid out before `envp`: `envp - 1` is argv's NULL
/// terminator and `argc` sits right before `argv[0]` (argv entries are
/// addresses, never equal to their count).
///
/// # Safety
/// `envp` is null or the `envp` of the process's initial stack layout.
#[cfg(unix)]
#[cfg_attr(not(target_env = "musl"), allow(dead_code))]
unsafe fn argv_before(envp: *const CStrPtr) -> Option<(i32, *const CStrPtr)> {
    if envp.is_null() {
        return None;
    }
    // SAFETY: the words before `envp` are argv's, then argc (caller).
    unsafe {
        let end = envp.sub(1);
        (0..1usize << 20).find_map(|n| {
            let slot = end.sub(n + 1);
            (*slot as usize == n).then(|| (i32::try_from(n).unwrap_or(0), slot.add(1)))
        })
    }
}

fn args() -> (usize, *const CStrPtr) {
    #[cfg(target_vendor = "apple")]
    {
        // SAFETY: set by dyld before `main`.
        unsafe { (*libc::_NSGetArgc() as usize, (*libc::_NSGetArgv()).cast()) }
    }
    #[cfg(all(unix, not(target_vendor = "apple")))]
    {
        // SAFETY: written once before `main`.
        let (c, v) = unsafe { ARGS };
        (c.max(0) as usize, v)
    }
    #[cfg(target_os = "wasi")]
    wasi_args()
}

#[cfg(target_os = "wasi")]
#[link(wasm_import_module = "wasi_snapshot_preview1")]
unsafe extern "C" {
    fn args_sizes_get(argc: *mut usize, size: *mut usize) -> u16;
    fn args_get(argv: *mut CStrPtr, buf: *mut u8) -> u16;
}

/// `args_get` once into a malloc block (never freed).
#[cfg(target_os = "wasi")]
fn wasi_args() -> (usize, *const CStrPtr) {
    static mut ARGS: (usize, *const CStrPtr) = (0, ptr::null());
    // SAFETY: one thread; the block holds `argc` pointers, then the
    // strings they point into.
    unsafe {
        if ARGS.1.is_null() {
            let (mut n, mut size) = (0, 0);
            if args_sizes_get(&mut n, &mut size) != 0 {
                return (0, ptr::null());
            }
            let v = libc::malloc((n + 1) * size_of::<CStrPtr>() + size).cast::<CStrPtr>();
            if v.is_null() {
                rokr_common::oom();
            }
            if args_get(v, v.add(n + 1).cast()) != 0 {
                return (0, ptr::null());
            }
            ARGS = (n, v);
        }
        ARGS
    }
}

/// C string `p` as a GC `str` (null → `""`).
fn from_c(p: CStrPtr) -> RokStr {
    if p.is_null() {
        return build(&[]);
    }
    // SAFETY: a NUL-terminated string from libc.
    build(&[unsafe { core::ffi::CStr::from_ptr(p) }.to_bytes()])
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_os_argc() -> usize {
    args().0
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_os_arg(i: usize) -> RokStr {
    let (n, v) = args();
    if i >= n || v.is_null() {
        return build(&[]);
    }
    // SAFETY: `argv` has `argc` entries.
    from_c(unsafe { *v.add(i) })
}

/// Entry `i` of `environ` (`"KEY=VALUE"`), `""` past the end.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_env_at(i: usize) -> RokStr {
    // SAFETY: `environ` is a null-terminated array; walk up to `i`.
    unsafe {
        let env = *environ();
        if env.is_null() {
            return build(&[]);
        }
        for k in 0..=i {
            if (*env.add(k)).is_null() {
                return build(&[]);
            }
        }
        from_c(*env.add(i))
    }
}

/// `getenv(name)` into `buf[off..off + len]`: its full length, or
/// `-ENOENT` when unset.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_getenv(name: RokStr, buf: *mut u8, off: usize, len: usize) -> isize {
    // SAFETY: `name` is a C string.
    let p = unsafe { libc::getenv(cs(name).p()) };
    if p.is_null() {
        return -(libc::ENOENT as isize);
    }
    // SAFETY: a NUL-terminated string from libc.
    put(
        buf,
        off,
        len,
        unsafe { core::ffi::CStr::from_ptr(p) }.to_bytes(),
    )
}

/// `setenv` (`value` given) / `unsetenv`. 0 or `-errno`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_setenv(name: RokStr, value: RokStr, unset: i32) -> i32 {
    // SAFETY: C strings. No global env lock (spec 306.3).
    let r = unsafe {
        if unset != 0 {
            libc::unsetenv(cs(name).p())
        } else {
            libc::setenv(cs(name).p(), cs(value).p(), 1)
        }
    };
    ret(r as isize) as i32
}

// Processes

/// Pointers to the NUL-separated pieces of `blob` (`count` of them),
/// null-terminated, in a malloc block.
#[cfg(unix)]
fn split_blob(blob: RokStr, count: usize) -> *mut CStrPtr {
    // SAFETY: malloc of `count + 1` pointers.
    let v = unsafe { libc::malloc((count + 1) * size_of::<CStrPtr>()) }.cast::<CStrPtr>();
    if v.is_null() {
        rokr_common::oom();
    }
    let mut p = blob.ptr;
    for i in 0..count {
        // SAFETY: the blob holds `count` NUL-terminated pieces.
        unsafe {
            *v.add(i) = p.cast();
            p = p.add(libc::strlen(p.cast()) + 1);
        }
    }
    // SAFETY: slot `count` exists.
    unsafe { *v.add(count) = ptr::null() };
    v
}

#[cfg(target_vendor = "apple")]
unsafe extern "C" {
    // libSystem since macOS 10.15; not in the `libc` crate for Apple.
    fn posix_spawn_file_actions_addchdir_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        path: CStrPtr,
    ) -> libc::c_int;
}
#[cfg(all(unix, not(target_vendor = "apple")))]
use libc::posix_spawn_file_actions_addchdir_np;

/// `posix_spawnp` (307 §3.7.4). `argv` / `envs` hold `argc` / `envc`
/// NUL-terminated pieces (`envc < 0`: inherit the environment; the
/// program is looked up on this process's `PATH` either way); `cwd`
/// empty = inherit; `pgid >= 0` joins that process group (0: a new one
/// led by the child). Stdio modes: `-1` inherit, `-2` null, `-3` piped,
/// `>= 0` that fd. The pipe ends (or -1) go to `out[0..3]` (stdin write
/// end, stdout / stderr read ends). Returns the pid or `-errno` (exec
/// failures included).
#[cfg(unix)]
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn rok_os_spawn(
    argv: RokStr,
    argc: usize,
    envs: RokStr,
    envc: isize,
    cwd: RokStr,
    stdio_in: i32,
    stdio_out: i32,
    stdio_err: i32,
    pgid: i32,
    out: *mut u8,
) -> i32 {
    let av = split_blob(argv, argc);
    let ev = if envc >= 0 {
        split_blob(envs, envc as usize)
    } else {
        ptr::null_mut()
    };
    let cwd_c = cs(cwd);
    let modes = [stdio_in, stdio_out, stdio_err];
    // [child end, parent end] per stream.
    let mut ends = [[-1i32; 2]; 3];
    let r = native(|| {
        // SAFETY: pipe / fcntl / posix_spawn on locals; the actions and
        // attributes are initialized before use and destroyed after.
        unsafe {
            for (i, m) in modes.iter().enumerate() {
                if *m == -3 {
                    let mut p = [0i32; 2];
                    if libc::pipe(p.as_mut_ptr()) < 0 {
                        return -errno();
                    }
                    // `dup2` onto 0..3 clears it on the child's copy.
                    libc::fcntl(p[0], libc::F_SETFD, libc::FD_CLOEXEC);
                    libc::fcntl(p[1], libc::F_SETFD, libc::FD_CLOEXEC);
                    ends[i] = if i == 0 { [p[0], p[1]] } else { [p[1], p[0]] };
                }
            }
            let mut fa: libc::posix_spawn_file_actions_t = core::mem::zeroed();
            let mut at: libc::posix_spawnattr_t = core::mem::zeroed();
            libc::posix_spawn_file_actions_init(&mut fa);
            libc::posix_spawnattr_init(&mut at);
            let mut rc = 0;
            for (i, m) in modes.iter().enumerate() {
                let fd = i as i32;
                rc = match *m {
                    -1 => 0,
                    -2 => {
                        let fl = if i == 0 {
                            libc::O_RDONLY
                        } else {
                            libc::O_WRONLY
                        };
                        libc::posix_spawn_file_actions_addopen(
                            &mut fa,
                            fd,
                            c"/dev/null".as_ptr(),
                            fl,
                            0,
                        )
                    }
                    -3 => libc::posix_spawn_file_actions_adddup2(&mut fa, ends[i][0], fd),
                    src => libc::posix_spawn_file_actions_adddup2(&mut fa, src, fd),
                };
                if rc != 0 {
                    break;
                }
            }
            if rc == 0 && cwd.len > 0 {
                rc = posix_spawn_file_actions_addchdir_np(&mut fa, cwd_c.p());
            }
            if rc == 0 && pgid >= 0 {
                rc = libc::posix_spawnattr_setflags(&mut at, libc::POSIX_SPAWN_SETPGROUP as _);
                if rc == 0 {
                    rc = libc::posix_spawnattr_setpgroup(&mut at, pgid);
                }
            }
            let mut pid: libc::pid_t = 0;
            if rc == 0 {
                let env = if ev.is_null() {
                    *environ()
                } else {
                    ev.cast_const()
                };
                rc = libc::posix_spawnp(&mut pid, *av, &fa, &at, av.cast(), env.cast());
            }
            libc::posix_spawn_file_actions_destroy(&mut fa);
            libc::posix_spawnattr_destroy(&mut at);
            for e in &ends {
                if e[0] >= 0 {
                    libc::close(e[0]);
                }
            }
            if rc != 0 { -rc } else { pid }
        }
    })
    .0;
    // SAFETY: malloc blocks from `split_blob`.
    unsafe {
        libc::free(av.cast());
        libc::free(ev.cast());
    }
    for (i, e) in ends.iter().enumerate() {
        if r < 0 && e[1] >= 0 {
            // SAFETY: our own pipe end.
            unsafe { libc::close(e[1]) };
        }
        // SAFETY: the library passes a live `GcBuf<i32>` of 3 elements.
        unsafe { *cell(out, i) = if r < 0 { -1 } else { e[1] } };
    }
    r
}

#[cfg(target_os = "wasi")]
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn rok_os_spawn(
    _argv: RokStr,
    _argc: usize,
    _envs: RokStr,
    _envc: isize,
    _cwd: RokStr,
    _stdio_in: i32,
    _stdio_out: i32,
    _stdio_err: i32,
    _pgid: i32,
    out: *mut u8,
) -> i32 {
    for i in 0..3 {
        // SAFETY: the library passes a live `GcBuf<i32>` of 3 elements.
        unsafe { *cell(out, i) = -1 };
    }
    UNSUPPORTED
}

/// `waitpid`: the pid (status in `out[0]`), 0 if `nohang` and still
/// running, or `-errno`. `nohang` bit 0: do not block; bit 1: only wait
/// for the end, leave the child to reap (`out` untouched).
#[cfg(unix)]
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_waitpid(pid: i32, nohang: i32, out: *mut u8) -> i32 {
    if nohang & 2 != 0 {
        return wait_ended(pid, nohang & 1 != 0);
    }
    let mut st = 0;
    let flags = if nohang != 0 { libc::WNOHANG } else { 0 };
    let (r, e) = native(|| {
        loop {
            // SAFETY: plain libc call.
            let r = unsafe { libc::waitpid(pid, &mut st, flags) };
            if r >= 0 || errno() != libc::EINTR {
                break r;
            }
        }
    });
    if r < 0 {
        return -e;
    }
    // SAFETY: a live one-element `GcBuf<i32>`.
    unsafe { *cell(out, 0) = st };
    r
}

/// `waitid(WNOWAIT)`: the pid once it has ended, still a zombie, 0 if
/// `nohang` and running, or `-errno`.
#[cfg(unix)]
fn wait_ended(pid: i32, nohang: bool) -> i32 {
    let flags = libc::WEXITED | libc::WNOWAIT | if nohang { libc::WNOHANG } else { 0 };
    let (r, e) = native(|| {
        loop {
            // SAFETY: a zeroed `siginfo_t` is valid; `waitid` fills it.
            let mut info: libc::siginfo_t = unsafe { core::mem::zeroed() };
            // SAFETY: plain libc call into `info`.
            let r = unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &raw mut info, flags) };
            if r >= 0 {
                // SAFETY: filled by `waitid`, or still zeroed (WNOHANG).
                break unsafe { info.si_pid() };
            }
            if errno() != libc::EINTR {
                break r;
            }
        }
    });
    if r < 0 { -e } else { r }
}

#[cfg(target_os = "wasi")]
#[unsafe(no_mangle)]
pub const extern "C" fn rok_os_waitpid(_pid: i32, _nohang: i32, _out: *mut u8) -> i32 {
    UNSUPPORTED
}

/// Write end of the signal pipe (`rok_os_signal_install`), or -1.
#[cfg(unix)]
static SIGNAL_PIPE: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(-1);

/// The C handler: the signal number as one byte down the pipe. Only
/// async-signal-safe calls; a full pipe drops the signal.
#[cfg(unix)]
extern "C" fn on_signal(sig: libc::c_int) {
    let fd = SIGNAL_PIPE.load(core::sync::atomic::Ordering::Relaxed);
    if fd >= 0 {
        let saved = errno();
        let b = sig as u8;
        // SAFETY: one byte from a local; `write` is async-signal-safe.
        unsafe {
            libc::write(fd, (&raw const b).cast(), 1);
            set_errno(saved);
        }
    }
}

/// Route signal `sig` to the signal pipe (`process::on_signal`). The first
/// call creates the pipe; `*read_end` gets its read end (blocking,
/// close-on-exec) on that call, -1 after. 0 or `-errno` (`EINVAL` for
/// `SIGKILL` / `SIGSTOP` / a bad number).
#[cfg(unix)]
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_signal_install(sig: i32, read_end: *mut u8) -> i32 {
    // SAFETY: the library passes a live `GcBuf<i32>` of 1 element.
    unsafe { *cell(read_end, 0) = -1 };
    if !(1..64).contains(&sig) || sig == libc::SIGKILL || sig == libc::SIGSTOP {
        return -libc::EINVAL;
    }
    if SIGNAL_PIPE.load(core::sync::atomic::Ordering::Acquire) < 0 {
        let mut p = [0i32; 2];
        // SAFETY: `p` is writable; fcntl on our own fds.
        unsafe {
            if libc::pipe(p.as_mut_ptr()) < 0 {
                return -errno();
            }
            for fd in p {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
            let fl = libc::fcntl(p[1], libc::F_GETFL);
            libc::fcntl(p[1], libc::F_SETFL, fl | libc::O_NONBLOCK);
            *cell(read_end, 0) = p[0];
        }
        SIGNAL_PIPE.store(p[1], core::sync::atomic::Ordering::Release);
    }
    // SAFETY: a zeroed `sigaction` with the fields set below is valid.
    unsafe {
        let mut sa: libc::sigaction = core::mem::zeroed();
        sa.sa_sigaction = on_signal as *const () as libc::sighandler_t;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        if libc::sigaction(sig, &sa, ptr::null_mut()) < 0 {
            return -errno();
        }
    }
    0
}

#[cfg(target_os = "wasi")]
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_signal_install(_sig: i32, read_end: *mut u8) -> i32 {
    // SAFETY: the library passes a live `GcBuf<i32>` of 1 element.
    unsafe { *cell(read_end, 0) = -1 };
    UNSUPPORTED
}

#[cfg(unix)]
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_raise(sig: i32) -> i32 {
    // SAFETY: plain libc call.
    if unsafe { libc::raise(sig) } != 0 {
        -libc::EINVAL
    } else {
        0
    }
}

#[cfg(target_os = "wasi")]
#[unsafe(no_mangle)]
pub const extern "C" fn rok_os_raise(_sig: i32) -> i32 {
    UNSUPPORTED
}

#[cfg(unix)]
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_kill(pid: i32, sig: i32) -> i32 {
    // SAFETY: plain libc call.
    ret(unsafe { libc::kill(pid, sig) } as isize) as i32
}

#[cfg(target_os = "wasi")]
#[unsafe(no_mangle)]
pub const extern "C" fn rok_os_kill(_pid: i32, _sig: i32) -> i32 {
    UNSUPPORTED
}

/// `pidfd_open(pid)` (Linux 5.3+): a close-on-exec fd tracking the
/// process, or `-errno`; `-ENOSYS` on other systems.
#[unsafe(no_mangle)]
#[cfg_attr(not(target_os = "linux"), allow(clippy::missing_const_for_fn))]
pub extern "C" fn rok_os_pidfd_open(pid: i32) -> i32 {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: plain syscall.
        ret(unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as isize) as i32
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        -libc::ENOSYS
    }
}

/// `pidfd_send_signal(fd, sig)`: 0 or `-errno` (`-ENOSYS` off Linux).
#[unsafe(no_mangle)]
#[cfg_attr(not(target_os = "linux"), allow(clippy::missing_const_for_fn))]
pub extern "C" fn rok_os_pidfd_signal(fd: i32, sig: i32) -> i32 {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: plain syscall; no siginfo.
        let r =
            unsafe { libc::syscall(libc::SYS_pidfd_send_signal, fd, sig, ptr::null::<u8>(), 0) };
        ret(r as isize) as i32
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (fd, sig);
        -libc::ENOSYS
    }
}

#[cfg(unix)]
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_getpid() -> u32 {
    // SAFETY: plain libc call.
    unsafe { libc::getpid() as u32 }
}

#[cfg(target_os = "wasi")]
#[unsafe(no_mangle)]
pub const extern "C" fn rok_os_getpid() -> u32 {
    0
}

#[cfg(unix)]
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_getppid() -> u32 {
    // SAFETY: plain libc call.
    unsafe { libc::getppid() as u32 }
}

#[cfg(target_os = "wasi")]
#[unsafe(no_mangle)]
pub const extern "C" fn rok_os_getppid() -> u32 {
    0
}

/// Abnormal termination (`SIGABRT`), no cleanup.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_abort() -> ! {
    // SAFETY: never returns.
    unsafe { libc::abort() }
}

// Randomness and trust anchors (crypto, net::tls)

/// Fill `buf[off..off + len]` from the OS random source (Linux
/// `getrandom`, else `getentropy`; WASI `random_get` underneath): 0 or
/// `-errno`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_random(buf: *mut u8, off: usize, len: usize) -> i32 {
    let mut done = 0;
    while done < len {
        // SAFETY: the library passes a live region of `len` bytes.
        let p = unsafe { region(buf, off + done) }.cast::<libc::c_void>();
        // SAFETY: `p` is writable for `len - done` bytes.
        #[cfg(target_os = "linux")]
        let r = unsafe { libc::getrandom(p, len - done, 0) };
        #[cfg(not(target_os = "linux"))]
        let r = {
            let n = (len - done).min(256);
            // SAFETY: `p` is writable for `n` bytes, getentropy's limit.
            if unsafe { libc::getentropy(p, n) } == 0 {
                n as isize
            } else {
                -1
            }
        };
        if r < 0 {
            let e = errno();
            if e != libc::EINTR {
                return -e;
            }
            continue;
        }
        done += r as usize;
    }
    0
}

/// The OS's trusted root certificates into `buf[off..off + len]` as
/// records `[u32 big-endian length][DER]`: their full length or `-errno`
/// (`-ENOSYS` where the OS has no certificate store API; the library
/// reads the bundle files there).
#[cfg(target_vendor = "apple")]
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_trust_roots(buf: *mut u8, off: usize, len: usize) -> isize {
    let (roots, _) = native(keychain::roots);
    match roots {
        Some(v) => put(buf, off, len, v.as_slice()),
        None => -(libc::ENOSYS as isize),
    }
}

#[cfg(not(target_vendor = "apple"))]
#[unsafe(no_mangle)]
pub const extern "C" fn rok_os_trust_roots(_buf: *mut u8, _off: usize, _len: usize) -> isize {
    -(libc::ENOSYS as isize)
}

/// Security.framework, loaded on first use (programs that never ask for
/// the roots do not link it).
#[cfg(target_vendor = "apple")]
mod keychain {
    use core::ffi::{CStr, c_void};
    use core::mem::transmute_copy;
    use core::ptr;

    use rokr_common::wtf8::MBuf;

    type Ref = *const c_void;

    struct Api {
        anchors: unsafe extern "C" fn(*mut Ref) -> i32,
        domain_certs: unsafe extern "C" fn(i32, *mut Ref) -> i32,
        settings: unsafe extern "C" fn(Ref, i32, *mut Ref) -> i32,
        cert_data: unsafe extern "C" fn(Ref) -> Ref,
        count: unsafe extern "C" fn(Ref) -> isize,
        at: unsafe extern "C" fn(Ref, isize) -> Ref,
        data_len: unsafe extern "C" fn(Ref) -> isize,
        data_ptr: unsafe extern "C" fn(Ref) -> *const u8,
        release: unsafe extern "C" fn(Ref),
    }

    /// Symbol `name` of `h` as the function pointer type `F`.
    ///
    /// # Safety
    /// `F` must be a function pointer type matching the symbol.
    unsafe fn sym<F>(h: *mut c_void, name: &CStr) -> Option<F> {
        // SAFETY: `h` is a live handle, `name` NUL-terminated.
        let p = unsafe { libc::dlsym(h, name.as_ptr()) };
        // SAFETY: caller contract; `F` is pointer-sized.
        (!p.is_null()).then(|| unsafe { transmute_copy::<*mut c_void, F>(&p) })
    }

    fn api() -> Option<Api> {
        // SAFETY: plain dlopen of a system framework.
        let h = unsafe {
            libc::dlopen(
                c"/System/Library/Frameworks/Security.framework/Security".as_ptr(),
                libc::RTLD_LAZY | libc::RTLD_LOCAL,
            )
        };
        if h.is_null() {
            return None;
        }
        // SAFETY: each symbol has the C signature declared in `Api`.
        unsafe {
            Some(Api {
                anchors: sym(h, c"SecTrustCopyAnchorCertificates")?,
                domain_certs: sym(h, c"SecTrustSettingsCopyCertificates")?,
                settings: sym(h, c"SecTrustSettingsCopyTrustSettings")?,
                cert_data: sym(h, c"SecCertificateCopyData")?,
                count: sym(h, c"CFArrayGetCount")?,
                at: sym(h, c"CFArrayGetValueAtIndex")?,
                data_len: sym(h, c"CFDataGetLength")?,
                data_ptr: sym(h, c"CFDataGetBytePtr")?,
                release: sym(h, c"CFRelease")?,
            })
        }
    }

    /// Append certificate `cert`'s DER as a record.
    fn push(a: &Api, out: &mut MBuf<u8>, cert: Ref) {
        // SAFETY: `cert` is a live SecCertificateRef; the copied data
        // is released after use.
        unsafe {
            let d = (a.cert_data)(cert);
            if d.is_null() {
                return;
            }
            let n = (a.data_len)(d) as usize;
            out.extend(&(n as u32).to_be_bytes());
            out.extend(core::slice::from_raw_parts((a.data_ptr)(d), n));
            (a.release)(d);
        }
    }

    /// The system anchors, plus the user (0) and admin (1) domains'
    /// certificates trusted unconditionally (an empty trust settings
    /// array). Certificates with qualified trust settings are left out.
    pub fn roots() -> Option<MBuf<u8>> {
        let a = api()?;
        let mut out = MBuf::new();
        // SAFETY: Core Foundation calls on objects this function owns
        // (copied) or borrows from them (array elements).
        unsafe {
            let mut arr: Ref = ptr::null();
            if (a.anchors)(&mut arr) == 0 && !arr.is_null() {
                for i in 0..(a.count)(arr) {
                    push(&a, &mut out, (a.at)(arr, i));
                }
                (a.release)(arr);
            }
            for domain in [0, 1] {
                let mut arr: Ref = ptr::null();
                if (a.domain_certs)(domain, &mut arr) != 0 || arr.is_null() {
                    continue;
                }
                for i in 0..(a.count)(arr) {
                    let cert = (a.at)(arr, i);
                    let mut s: Ref = ptr::null();
                    if (a.settings)(cert, domain, &mut s) == 0 && !s.is_null() {
                        let unconditional = (a.count)(s) == 0;
                        (a.release)(s);
                        if unconditional {
                            push(&a, &mut out, cert);
                        }
                    }
                }
                (a.release)(arr);
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_kinds() {
        assert_eq!(rok_os_errno_kind(libc::ENOENT), kind::NOT_FOUND);
        assert_eq!(rok_os_errno_kind(libc::EPERM), kind::PERMISSION_DENIED);
        assert_eq!(rok_os_errno_kind(libc::EACCES), kind::PERMISSION_DENIED);
        assert_eq!(rok_os_errno_kind(libc::EEXIST), kind::ALREADY_EXISTS);
        assert_eq!(rok_os_errno_kind(libc::ENOTSUP), kind::UNSUPPORTED);
        assert_eq!(
            rok_os_errno_kind(EAI_BASE + libc::EAI_NONAME.abs()),
            kind::NOT_FOUND
        );
        assert_eq!(
            rok_os_errno_kind(EAI_BASE + libc::EAI_AGAIN.abs()),
            kind::OTHER
        );
        assert_eq!(rok_os_errno_kind(0), kind::OTHER);
        assert_eq!(rok_os_errno_kind(-1), kind::OTHER);
    }

    /// The initial stack: `argc, argv.., NULL, envp.., NULL`.
    #[test]
    fn argv_from_the_initial_stack() {
        let (prog, flag, var) = (c"prog".as_ptr(), c"-x".as_ptr(), c"K=V".as_ptr());
        for (stack, envp_at, argc) in [
            (
                vec![2 as CStrPtr, prog, flag, ptr::null(), var, ptr::null()],
                4,
                2,
            ),
            (vec![0 as CStrPtr, ptr::null(), var, ptr::null()], 2, 0),
        ] {
            // SAFETY: `stack` has that layout.
            let (count, argv) = unsafe { argv_before(stack.as_ptr().add(envp_at)) }.unwrap();
            assert_eq!(count, argc);
            assert_eq!(argv, stack[1..].as_ptr());
        }
        // SAFETY: null is allowed.
        assert!(unsafe { argv_before(ptr::null()) }.is_none());
    }
}
