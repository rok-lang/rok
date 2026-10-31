//! Win32 helpers behind `rok::std` (stdlib 300: fs, io, env, process).
//! No ABI stability (805 §8.5.12 appendix). The same `rok_os_*` symbols
//! and conventions as the POSIX `os`, with these differences:
//!
//! - An "fd" is a handle value (0 / 1 / 2: the standard handles). A
//!   socket has bit 0 set ([`sock_of`]) so `close` / `read` / `write` /
//!   `dup` can tell.
//! - Errors are negated Win32 / Winsock codes.
//! - Text is WTF-8 on the rok side, UTF-16 on the Windows side.
//! - A directory capability is a handle; paths under it resolve
//!   lexically against its final path.
//! - A child's process handle is kept until it is reaped, so its pid is
//!   not reused meanwhile.
//! - Signals are console events: Ctrl-C 2, Ctrl-Break 21, close 1.

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::mem::{size_of, zeroed};
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};

pub use rokr_common::errno;
use rokr_common::object_header::HEADER_SIZE;
use rokr_common::sys::{self, MBuf, Wide, fill_wide, narrow, push_wide, wide};
use rokr_common::win::{self, DWORD, HANDLE};

use crate::str::{RokStr, build};
use crate::winargs::{Args, parse_command_line, push_arg};

/// Run a blocking call parked; the last error survives the re-entry.
pub fn native<R>(f: impl FnOnce() -> R) -> (R, i32) {
    crate::lane::block_begin();
    let r = f();
    let e = errno();
    crate::lane::block_end();
    (r, e)
}

/// `-GetLastError()`.
fn fail() -> i32 {
    -errno()
}

/// Whether `hd` was opened for overlapped I/O (the parent's end of a
/// child's output pipe, [`output_pipe`]).
fn overlapped(hd: HANDLE) -> bool {
    // FILE_SYNCHRONOUS_IO_ALERT | _NONALERT.
    const SYNCHRONOUS: u32 = 0x30;
    let mut mode = 0u32;
    // SAFETY: zeroed status block; a 4-byte FileModeInformation (16).
    let s = unsafe {
        let mut iosb: win::IO_STATUS_BLOCK = core::mem::zeroed();
        win::NtQueryInformationFile(hd, &mut iosb, (&raw mut mode).cast(), 4, 16)
    };
    s >= 0 && mode & SYNCHRONOUS == 0
}

/// `ReadFile` of `want` bytes into `dst` (0: until data is there or the
/// writer closed), which waits for its own request on an overlapped
/// handle. The last error is the read's.
///
/// # Safety
/// `dst` must be writable for `want` bytes.
unsafe fn read_handle(hd: HANDLE, dst: *mut u8, want: DWORD, n: &mut DWORD) -> win::BOOL {
    if !overlapped(hd) {
        // SAFETY: caller contract.
        return unsafe { win::ReadFile(hd, dst.cast(), want, n, ptr::null_mut()) };
    }
    // SAFETY: a fresh event and request, both done with before returning.
    unsafe {
        let mut ov: win::OVERLAPPED = core::mem::zeroed();
        ov.hEvent = win::CreateEventW(ptr::null_mut(), 1, 0, ptr::null());
        let mut ok = win::ReadFile(hd, dst.cast(), want, n, &mut ov);
        if ok == 0 && errno() == win::ERROR_IO_PENDING {
            ok = win::GetOverlappedResult(hd, &mut ov, n, 1);
        }
        let e = win::GetLastError();
        win::CloseHandle(ov.hEvent);
        win::SetLastError(e);
        ok
    }
}

/// A pipe for a child's output: (our read end, overlapped, so
/// [`rok_os_poll_read`] waits on two at once; the child's write end).
fn output_pipe() -> Result<(HANDLE, HANDLE), i32> {
    static N: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    // `\\.\pipe\rok-<pid>-<n>`, NUL-terminated.
    let mut name = [0u16; 48];
    let prefix = ascii_w(br"\\.\pipe\rok-");
    name[..prefix.len()].copy_from_slice(&prefix);
    let mut at = prefix.len();
    // SAFETY: plain call.
    let pid = unsafe { win::GetCurrentProcessId() };
    for (j, v) in [pid, N.fetch_add(1, Ordering::Relaxed)]
        .into_iter()
        .enumerate()
    {
        if j > 0 {
            name[at] = u16::from(b'-');
            at += 1;
        }
        let mut digits = [0u16; 10];
        let mut i = digits.len();
        let mut v = v;
        loop {
            i -= 1;
            digits[i] = u16::from(b'0') + (v % 10) as u16;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        name[at..at + digits.len() - i].copy_from_slice(&digits[i..]);
        at += digits.len() - i;
    }
    // SAFETY: a NUL-terminated name; one inbound byte-mode instance.
    unsafe {
        let ours = win::CreateNamedPipeW(
            name.as_ptr(),
            win::PIPE_ACCESS_INBOUND
                | win::FILE_FLAG_OVERLAPPED
                | win::FILE_FLAG_FIRST_PIPE_INSTANCE,
            win::PIPE_REJECT_REMOTE_CLIENTS,
            1,
            1 << 16,
            1 << 16,
            0,
            ptr::null_mut(),
        );
        if ours == win::INVALID_HANDLE_VALUE {
            return Err(fail());
        }
        let child = win::CreateFileW(
            name.as_ptr(),
            win::GENERIC_WRITE,
            0,
            ptr::null_mut(),
            win::OPEN_EXISTING,
            0,
            0,
        );
        if child == win::INVALID_HANDLE_VALUE {
            let e = fail();
            win::CloseHandle(ours);
            return Err(e);
        }
        Ok((ours, child))
    }
}

/// The writable region `off..off + len` of the `GcBuf<u8>` `buf`.
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + len` bytes.
pub const unsafe fn region(buf: *mut u8, off: usize) -> *mut u8 {
    // SAFETY: caller contract.
    unsafe { buf.add(HEADER_SIZE + off) }
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

/// Copy up to `len` bytes of `src` to `buf[off..]`; `src`'s full length.
fn put(buf: *mut u8, off: usize, len: usize, src: &[u8]) -> isize {
    // SAFETY: the library passes a live region of `len` bytes.
    unsafe { ptr::copy_nonoverlapping(src.as_ptr(), region(buf, off), src.len().min(len)) };
    src.len() as isize
}

/// [`put`] for a UTF-16 result.
fn put_wide(buf: *mut u8, off: usize, len: usize, w: &[u16]) -> isize {
    put(buf, off, len, narrow(w).as_slice())
}

/// The handle behind `fd`.
fn h(fd: i32) -> HANDLE {
    sys::handle(fd)
}

/// The socket behind `fd`, if it is one (see the module notes).
pub const fn sock_of(fd: i32) -> Option<win::SOCKET> {
    if fd != 1 && fd & 3 == 1 {
        Some((fd & !3) as u32 as win::SOCKET)
    } else {
        None
    }
}

/// The fd for socket `s`.
pub const fn fd_of(s: win::SOCKET) -> i32 {
    s as i32 | 1
}

// Errors

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

/// The `IoErrorKind` discriminant of Win32 / Winsock error `code`.
#[unsafe(no_mangle)]
#[allow(clippy::wildcard_imports)] // the error-code constants
pub const extern "C" fn rok_os_errno_kind(code: i32) -> i32 {
    use win::*;
    match code {
        ERROR_FILE_NOT_FOUND
        | ERROR_PATH_NOT_FOUND
        | ERROR_INVALID_DRIVE
        | ERROR_ENVVAR_NOT_FOUND
        | WSAHOST_NOT_FOUND
        | WSANO_DATA => kind::NOT_FOUND,
        ERROR_ACCESS_DENIED | ERROR_WRITE_PROTECT | ERROR_PRIVILEGE_NOT_HELD | WSAEACCES => {
            kind::PERMISSION_DENIED
        }
        ERROR_FILE_EXISTS | ERROR_ALREADY_EXISTS => kind::ALREADY_EXISTS,
        ERROR_DIRECTORY => kind::NOT_A_DIRECTORY,
        ERROR_DIRECTORY_NOT_SUPPORTED => kind::IS_A_DIRECTORY,
        ERROR_DIR_NOT_EMPTY => kind::DIRECTORY_NOT_EMPTY,
        ERROR_INVALID_PARAMETER | ERROR_INVALID_NAME | WSAEINVAL => kind::INVALID_INPUT,
        ERROR_BROKEN_PIPE | ERROR_NO_DATA | WSAESHUTDOWN => kind::BROKEN_PIPE,
        WSAEINTR => kind::INTERRUPTED,
        WSAEWOULDBLOCK => kind::WOULD_BLOCK,
        ERROR_TIMEOUT | WSAETIMEDOUT => kind::TIMED_OUT,
        ERROR_HANDLE_DISK_FULL | ERROR_DISK_FULL => kind::STORAGE_FULL,
        ERROR_NOT_SAME_DEVICE => kind::CROSSES_DEVICES,
        ERROR_NOT_SUPPORTED | ERROR_CALL_NOT_IMPLEMENTED | WSAEOPNOTSUPP => kind::UNSUPPORTED,
        _ => kind::OTHER,
    }
}

/// The system message of error `code` (`FormatMessageW`).
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_strerror(code: i32) -> RokStr {
    let mut buf = [0u16; 512];
    // SAFETY: `buf` is writable for its length; no inserts.
    let n = unsafe {
        win::FormatMessageW(
            win::FORMAT_MESSAGE_FROM_SYSTEM | win::FORMAT_MESSAGE_IGNORE_INSERTS,
            ptr::null(),
            code as DWORD,
            0,
            buf.as_mut_ptr(),
            buf.len() as DWORD,
            ptr::null_mut(),
        )
    } as usize;
    let mut msg = &buf[..n];
    while let [rest @ .., last] = msg {
        if !matches!(u8::try_from(*last), Ok(b'\r' | b'\n' | b' ' | b'.')) {
            break;
        }
        msg = rest;
    }
    if msg.is_empty() {
        let mut d = [0u8; 12];
        return build(&[b"os error ", decimal(code, &mut d)]);
    }
    build(&[narrow(msg).as_slice()])
}

/// `v` in decimal.
fn decimal(v: i32, out: &mut [u8; 12]) -> &[u8] {
    let mut n = v.unsigned_abs();
    let mut i = out.len();
    loop {
        i -= 1;
        out[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    if v < 0 {
        i -= 1;
        out[i] = b'-';
    }
    &out[i..]
}

// Paths

/// `path` under directory capability `dir` (negative: the process's
/// own resolution, relative to the working directory) as a Win32 path.
fn path_w(dir: i32, path: RokStr) -> Result<Wide, i32> {
    if dir < 0 {
        return Ok(wide(path.bytes()));
    }
    let mut w = final_path(h(dir))?;
    for part in path.bytes().split(|b| *b == b'/' || *b == b'\\') {
        match part {
            b"" | b"." => {}
            b".." => {
                // Never above the capability's own path.
                if let Some(i) = w.as_slice().iter().rposition(|&u| u == u16::from(b'\\')) {
                    w.truncate(i);
                }
            }
            _ => {
                w.push(u16::from(b'\\'));
                push_wide(&mut w, part);
            }
        }
    }
    Ok(w)
}

/// The `\\?\` final path of the open handle `h`.
fn final_path(h: HANDLE) -> Result<Wide, i32> {
    let mut w = fill_wide(|b, n| {
        // SAFETY: `b` is writable for `n` units.
        unsafe { win::GetFinalPathNameByHandleW(h, b, n, win::FILE_NAME_NORMALIZED) }
    })?;
    if w.as_slice().last() == Some(&u16::from(b'\\')) {
        let n = w.len() - 1;
        w.truncate(n);
    }
    Ok(w)
}

/// A `\\?\` path as the user would write it: `\\?\C:\x` → `C:\x`,
/// `\\?\UNC\srv\x` → `\\srv\x`.
fn friendly(w: &[u16]) -> MBuf<u16> {
    let mut out = MBuf::new();
    let unc = ascii_w(br"\\?\UNC\");
    let verbatim = ascii_w(br"\\?\");
    if w.starts_with(&unc) {
        out.extend(&[u16::from(b'\\'); 2]);
        out.extend(&w[unc.len()..]);
    } else if w.starts_with(&verbatim)
        && w.len() >= 6
        && w[5] == u16::from(b':')
        && u8::try_from(w[4]).is_ok_and(|c| c.is_ascii_alphabetic())
    {
        out.extend(&w[verbatim.len()..]);
    } else {
        out.extend(w);
    }
    out
}

/// The UTF-16 of an ASCII literal.
pub const fn ascii_w<const N: usize>(s: &[u8; N]) -> [u16; N] {
    let mut a = [0u16; N];
    let mut i = 0;
    while i < N {
        a[i] = s[i] as u16;
        i += 1;
    }
    a
}

/// An open handle closed on drop.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: our own handle.
        unsafe { win::CloseHandle(self.0) };
    }
}

/// Open `path` for metadata only (`follow == false`: a symlink itself).
fn open_meta(path: &mut Wide, follow: bool) -> Result<Owned, i32> {
    let flags = win::FILE_FLAG_BACKUP_SEMANTICS
        | if follow {
            0
        } else {
            win::FILE_FLAG_OPEN_REPARSE_POINT
        };
    // SAFETY: a NUL-terminated path.
    let hd = unsafe {
        win::CreateFileW(
            path.ptr(),
            win::FILE_READ_ATTRIBUTES,
            win::FILE_SHARE_READ | win::FILE_SHARE_WRITE | win::FILE_SHARE_DELETE,
            ptr::null_mut(),
            win::OPEN_EXISTING,
            flags,
            0,
        )
    };
    if hd == win::INVALID_HANDLE_VALUE {
        Err(fail())
    } else {
        Ok(Owned(hd))
    }
}

// Handles

/// `FILE_GENERIC_WRITE` without `FILE_WRITE_DATA`: appends only.
const APPEND_ACCESS: DWORD = 0x0012_0114;

/// `CreateFileW`. `flags` bits: 1 read, 2 write, 4 append, 8 truncate,
/// 16 create, 32 `create_new`, 64 directory. A created file is read-only
/// when `mode` has no write bit. Never inherited by children.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_open(dir: i32, path: RokStr, flags: i32, mode: u32) -> i32 {
    let mut access = if flags & 1 != 0 { win::GENERIC_READ } else { 0 };
    if flags & 4 != 0 {
        access |= APPEND_ACCESS;
    } else if flags & 2 != 0 {
        access |= win::GENERIC_WRITE;
    }
    let disposition = match (flags & 32 != 0, flags & 16 != 0, flags & 8 != 0) {
        (true, _, _) => win::CREATE_NEW,
        (_, true, true) => win::CREATE_ALWAYS,
        (_, true, false) => win::OPEN_ALWAYS,
        (_, false, true) => win::TRUNCATE_EXISTING,
        _ => win::OPEN_EXISTING,
    };
    let dir_flag = flags & 64 != 0;
    let mut attrs = if mode & 0o222 == 0 && flags & 48 != 0 {
        win::FILE_ATTRIBUTE_READONLY
    } else {
        win::FILE_ATTRIBUTE_NORMAL
    };
    if dir_flag {
        attrs |= win::FILE_FLAG_BACKUP_SEMANTICS;
    }
    let mut p = match path_w(dir, path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    // SAFETY: a NUL-terminated path.
    let (hd, e) = native(|| unsafe {
        win::CreateFileW(
            p.ptr(),
            access,
            win::FILE_SHARE_READ | win::FILE_SHARE_WRITE | win::FILE_SHARE_DELETE,
            ptr::null_mut(),
            disposition,
            attrs,
            0,
        )
    });
    if hd == win::INVALID_HANDLE_VALUE {
        return -e;
    }
    if dir_flag && !is_dir(hd) {
        // SAFETY: ours.
        unsafe { win::CloseHandle(hd) };
        return -win::ERROR_DIRECTORY;
    }
    hd as i32
}

/// Whether the open handle `hd` is a directory.
fn is_dir(hd: HANDLE) -> bool {
    // SAFETY: zeroed info is a valid out-buffer.
    let mut si: win::FILE_STANDARD_INFO = unsafe { zeroed() };
    // SAFETY: valid out-buffer of its size.
    let ok = unsafe {
        win::GetFileInformationByHandleEx(
            hd,
            win::FileStandardInfo,
            (&raw mut si).cast(),
            size_of::<win::FILE_STANDARD_INFO>() as DWORD,
        )
    };
    ok != 0 && si.Directory != 0
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_os_close(fd: i32) -> i32 {
    if let Some(s) = sock_of(fd) {
        return crate::net::close_socket(s);
    }
    // SAFETY: closing a handle the library owns.
    if unsafe { win::CloseHandle(h(fd)) } == 0 {
        fail()
    } else {
        0
    }
}

/// One read into `buf[off..off + len]`: the count (0 = EOF, including a
/// pipe whose writer closed) or `-error`.
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_os_read(fd: i32, buf: *mut u8, off: usize, len: usize) -> isize {
    // SAFETY: caller contract.
    let dst = unsafe { region(buf, off) };
    if let Some(s) = sock_of(fd) {
        return crate::net::recv(s, dst, len);
    }
    if fd == 0 {
        return crate::stdio::read0(dst, len);
    }
    let mut n = 0;
    let want = len.min(1 << 30) as DWORD;
    // SAFETY: `dst` is writable for `want` bytes.
    let (ok, e) = native(|| unsafe { read_handle(h(fd), dst, want, &mut n) });
    match (ok, e) {
        (0, win::ERROR_BROKEN_PIPE) => 0,
        (0, e) => -(e as isize),
        _ => n as isize,
    }
}

/// One write of `buf[off..off + len]`: the count or `-error`.
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_os_write(fd: i32, buf: *mut u8, off: usize, len: usize) -> isize {
    // SAFETY: caller contract.
    let src = unsafe { region(buf, off) };
    if let Some(s) = sock_of(fd) {
        return crate::net::send(s, src, len);
    }
    let hd = h(fd);
    if sys::is_console(hd) {
        // SAFETY: `src` is readable for `len` bytes.
        let bytes = unsafe { core::slice::from_raw_parts(src, len) };
        let r = native(|| sys::write_all(fd, &[bytes])).0;
        return if r < 0 { r as isize } else { len as isize };
    }
    let mut n = 0;
    let want = len.min(1 << 30) as DWORD;
    // SAFETY: `src` is readable for `want` bytes.
    let (ok, e) =
        native(|| unsafe { win::WriteFile(hd, src.cast(), want, &mut n, ptr::null_mut()) });
    if ok == 0 { -(e as isize) } else { n as isize }
}

/// Wait until pipe `fd1` or `fd2` (negative: ignored) has data or its
/// writer closed: bit 0 / bit 1 set for each ready one, or `-error`.
///
/// A child's output pipes are overlapped ([`output_pipe`]): a zero-byte
/// read on each completes when data arrives or the writer closes, and
/// the wait takes whichever comes first. A handle that is not an
/// overlapped pipe counts as ready (a read just blocks).
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_poll_read(fd1: i32, fd2: i32) -> i32 {
    let fds = [fd1, fd2];
    // Data or the writer gone, or a handle this cannot wait on.
    let ready = |fd: i32| {
        if fd < 0 {
            return false;
        }
        let mut avail = 0;
        // SAFETY: valid out-pointer; no data is read.
        let ok = unsafe {
            win::PeekNamedPipe(
                h(fd),
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut avail,
                ptr::null_mut(),
            )
        };
        ok == 0 || avail > 0 || !overlapped(h(fd))
    };
    native(|| {
        loop {
            let r = i32::from(ready(fd1)) | (i32::from(ready(fd2)) << 1);
            if r != 0 || (fd1 < 0 && fd2 < 0) {
                break r;
            }
            // SAFETY: zeroed requests; each is finished (waited for or
            // cancelled and waited for) before its event closes.
            unsafe {
                let mut ovs: [win::OVERLAPPED; 2] = core::mem::zeroed();
                let mut events = [0 as HANDLE; 2];
                let mut pending = [false; 2];
                let mut k = 0;
                let mut dummy = 0u8;
                for (i, &fd) in fds.iter().enumerate() {
                    if fd < 0 {
                        continue;
                    }
                    ovs[i].hEvent = win::CreateEventW(ptr::null_mut(), 1, 0, ptr::null());
                    let mut n = 0;
                    let ok = win::ReadFile(h(fd), (&raw mut dummy).cast(), 0, &mut n, &mut ovs[i]);
                    if ok == 0 && errno() == win::ERROR_IO_PENDING {
                        pending[i] = true;
                        events[k] = ovs[i].hEvent;
                        k += 1;
                    }
                }
                if pending.iter().filter(|&&p| p).count() == fds.iter().filter(|&&f| f >= 0).count()
                {
                    win::WaitForMultipleObjects(k as DWORD, events.as_ptr(), 0, win::INFINITE);
                }
                for (i, &fd) in fds.iter().enumerate() {
                    if fd < 0 {
                        continue;
                    }
                    if pending[i] {
                        let mut n = 0;
                        win::CancelIoEx(h(fd), &mut ovs[i]);
                        win::GetOverlappedResult(h(fd), &mut ovs[i], &mut n, 1);
                    }
                    win::CloseHandle(ovs[i].hEvent);
                }
            }
        }
    })
    .0
}

/// `FlushFileBuffers` (data and metadata either way).
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_fsync(fd: i32, _data_only: i32) -> i32 {
    // SAFETY: plain call.
    let (ok, e) = native(|| unsafe { win::FlushFileBuffers(h(fd)) });
    if ok == 0 { -e } else { 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_os_ftruncate(fd: i32, len: u64) -> i32 {
    let end = len as i64;
    // SAFETY: an 8-byte `FILE_END_OF_FILE_INFO`.
    let ok = unsafe {
        win::SetFileInformationByHandle(h(fd), win::FileEndOfFileInfo, (&raw const end).cast(), 8)
    };
    if ok == 0 { fail() } else { 0 }
}

/// `SetFilePointerEx`; `whence` 0 start, 1 current, 2 end (the Win32
/// numbering too). New offset or `-error`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_lseek(fd: i32, off: i64, whence: i32) -> i64 {
    let mut at = 0;
    // SAFETY: valid out-pointer.
    if unsafe { win::SetFilePointerEx(h(fd), off, &mut at, whence.clamp(0, 2) as DWORD) } == 0 {
        return i64::from(fail());
    }
    at
}

/// A second handle to the same file (or socket), not inherited.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_dup(fd: i32) -> i32 {
    if let Some(s) = sock_of(fd) {
        return crate::net::dup_socket(s);
    }
    let mut out = 0;
    // SAFETY: duplicates within this process.
    let ok = unsafe {
        let me = win::GetCurrentProcess();
        win::DuplicateHandle(me, h(fd), me, &mut out, 0, 0, win::DUPLICATE_SAME_ACCESS)
    };
    if ok == 0 { fail() } else { out as i32 }
}

/// Permissions as far as Windows has them: no write bit for anyone =
/// the read-only attribute. Goes through a second handle with
/// attribute-write access, so `fd`'s own access does not matter.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_fchmod(fd: i32, mode: u32) -> i32 {
    // SAFETY: a fresh handle to the same file, closed by `Owned`.
    let hd = unsafe {
        win::ReOpenFile(
            h(fd),
            win::FILE_READ_ATTRIBUTES | win::FILE_WRITE_ATTRIBUTES,
            win::FILE_SHARE_READ | win::FILE_SHARE_WRITE | win::FILE_SHARE_DELETE,
            0,
        )
    };
    if hd == win::INVALID_HANDLE_VALUE {
        return fail();
    }
    let hd = Owned(hd);
    // SAFETY: zeroed info is a valid out-buffer.
    let mut bi: win::FILE_BASIC_INFO = unsafe { zeroed() };
    let n = size_of::<win::FILE_BASIC_INFO>() as DWORD;
    // SAFETY: valid buffers of their size.
    unsafe {
        if win::GetFileInformationByHandleEx(hd.0, win::FileBasicInfo, (&raw mut bi).cast(), n) == 0
        {
            return fail();
        }
        if mode & 0o222 == 0 {
            bi.FileAttributes |= win::FILE_ATTRIBUTE_READONLY;
        } else {
            bi.FileAttributes &= !win::FILE_ATTRIBUTE_READONLY;
        }
        // Zero times mean "unchanged".
        bi.CreationTime = 0;
        bi.LastAccessTime = 0;
        bi.LastWriteTime = 0;
        bi.ChangeTime = 0;
        if win::SetFileInformationByHandle(hd.0, win::FileBasicInfo, (&raw const bi).cast(), n) == 0
        {
            return fail();
        }
    }
    0
}

/// The allocation granularity (64 KiB): map view offsets are multiples.
fn granularity() -> usize {
    static G: AtomicUsize = AtomicUsize::new(0);
    let g = G.load(Ordering::Relaxed);
    if g != 0 {
        return g;
    }
    // SAFETY: zeroed info is a valid out-buffer.
    let mut si: win::SYSTEM_INFO = unsafe { zeroed() };
    // SAFETY: valid out-pointer.
    unsafe { win::GetSystemInfo(&mut si) };
    let g = si.dwAllocationGranularity as usize;
    G.store(g, Ordering::Relaxed);
    g
}

/// A read-only view of `len` bytes of `fd` at `offset`: its address, or
/// `-error`. The view starts at the granularity boundary below `offset`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_mmap(fd: i32, len: usize, offset: u64) -> i64 {
    if len == 0 {
        return -i64::from(win::ERROR_INVALID_PARAMETER);
    }
    let delta = (offset % granularity() as u64) as usize;
    let base = offset - delta as u64;
    // SAFETY: a mapping object for the file, closed once the view holds
    // its own reference.
    let (p, e) = native(|| unsafe {
        let m = win::CreateFileMappingW(
            h(fd),
            ptr::null_mut(),
            win::PAGE_READONLY,
            0,
            0,
            ptr::null(),
        );
        if m == 0 {
            return ptr::null_mut();
        }
        let p = win::MapViewOfFile(
            m,
            win::FILE_MAP_READ,
            (base >> 32) as DWORD,
            base as DWORD,
            len + delta,
        );
        let e = errno();
        win::CloseHandle(m);
        win::SetLastError(e as DWORD);
        p
    });
    if p.is_null() {
        -i64::from(e)
    } else {
        (p as usize + delta) as i64
    }
}

/// Unmap a `rok_os_mmap` view: 0 or `-error`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_munmap(addr: usize, _len: usize) -> i32 {
    let base = addr & !(granularity() - 1);
    // SAFETY: `base` is the view `rok_os_mmap` mapped.
    if unsafe { win::UnmapViewOfFile(base as *const c_void) } == 0 {
        fail()
    } else {
        0
    }
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
    unsafe { ptr::copy_nonoverlapping((addr + at) as *const u8, region(buf, off), len) };
}

/// Whole-file lock: `op` 1 shared, 2 exclusive, 8 unlock, | 4
/// non-blocking. 1 = locked, 0 = would block (non-blocking), `-error`.
/// Like `flock`, changing a held lock's kind unlocks first.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_flock(fd: i32, op: i32) -> i32 {
    let hd = h(fd);
    // SAFETY: zeroed OVERLAPPED = offset 0; the handle is synchronous.
    let mut ov: win::OVERLAPPED = unsafe { zeroed() };
    // SAFETY: as above; unlocking an unlocked range just fails.
    unsafe { win::UnlockFileEx(hd, 0, u32::MAX, u32::MAX, &mut ov) };
    if op & 8 != 0 {
        return 1;
    }
    let mut flags = 0;
    if op & 2 != 0 {
        flags |= win::LOCKFILE_EXCLUSIVE_LOCK;
    }
    if op & 4 != 0 {
        flags |= win::LOCKFILE_FAIL_IMMEDIATELY;
    }
    // SAFETY: as above.
    let (ok, e) = native(|| unsafe { win::LockFileEx(hd, flags, 0, u32::MAX, u32::MAX, &mut ov) });
    match (ok, e) {
        (0, win::ERROR_LOCK_VIOLATION) if op & 4 != 0 => 0,
        (0, e) => -e,
        _ => 1,
    }
}

/// Copy `src` → `dst` from their current offsets to EOF: bytes or `-error`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_copy_fd(src: i32, dst: i32) -> i64 {
    let (r, e) = native(|| {
        let mut buf = [0u8; 16 * 1024];
        let mut total: i64 = 0;
        loop {
            let mut n = 0;
            // SAFETY: `buf` is writable for its length.
            let ok = unsafe { read_handle(h(src), buf.as_mut_ptr(), buf.len() as DWORD, &mut n) };
            if ok == 0 && errno() != win::ERROR_BROKEN_PIPE {
                return -1;
            }
            if ok == 0 || n == 0 {
                return total;
            }
            if sys::write_handle(h(dst), &buf[..n as usize]) < 0 {
                return -1;
            }
            total += i64::from(n);
        }
    });
    if r < 0 { -i64::from(e) } else { r }
}

// Paths and directories

/// Copy `from` to a new `to`, both under `dir` (`CopyFileExW`: block
/// cloning where the volume supports it, attributes included). 0 or
/// `-error` (`to` exists, ...) - the caller then copies the bytes.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_clone(dir: i32, from: RokStr, to: RokStr) -> i32 {
    let (mut a, mut b) = match (path_w(dir, from), path_w(dir, to)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    // SAFETY: NUL-terminated paths; no progress routine.
    let (ok, e) = native(|| unsafe {
        win::CopyFileExW(
            a.ptr(),
            b.ptr(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            win::COPY_FILE_FAIL_IF_EXISTS,
        )
    });
    if ok == 0 { -e } else { 0 }
}

/// 100 ns ticks since 1601 as `(unix seconds, nanoseconds)`.
const fn unix_time(ticks: i64) -> (i64, i64) {
    let t = ticks - sys::FILETIME_UNIX;
    (t.div_euclid(10_000_000), t.rem_euclid(10_000_000) * 100)
}

/// Metadata of `fd` (`>= 0`) or of `path` under `dir`, filling the
/// `GcBuf<i64>` `out`: `[kind, size, mode, mtime_s, mtime_ns, atime_s,
/// ctime_s, atime_ns]`, kind 1 file, 2 dir, 3 symlink (or junction).
/// `mode` is synthesized: `r` for all, `w` unless read-only, `x` on
/// directories. 0 or `-error`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_stat(dir: i32, path: RokStr, follow: i32, fd: i32, out: *mut u8) -> i32 {
    let owned;
    let hd = if fd >= 0 {
        h(fd)
    } else {
        let mut p = match path_w(dir, path) {
            Ok(p) => p,
            Err(e) => return e,
        };
        owned = match native(|| open_meta(&mut p, follow != 0)).0 {
            Ok(o) => o,
            Err(e) => return e,
        };
        owned.0
    };
    // SAFETY: zeroed infos are valid out-buffers of their sizes.
    let (mut bi, mut si, mut ti): (
        win::FILE_BASIC_INFO,
        win::FILE_STANDARD_INFO,
        win::FILE_ATTRIBUTE_TAG_INFO,
    ) = unsafe { (zeroed(), zeroed(), zeroed()) };
    // SAFETY: as above.
    let ok = unsafe {
        win::GetFileInformationByHandleEx(
            hd,
            win::FileBasicInfo,
            (&raw mut bi).cast(),
            size_of::<win::FILE_BASIC_INFO>() as DWORD,
        ) != 0
            && win::GetFileInformationByHandleEx(
                hd,
                win::FileStandardInfo,
                (&raw mut si).cast(),
                size_of::<win::FILE_STANDARD_INFO>() as DWORD,
            ) != 0
            && win::GetFileInformationByHandleEx(
                hd,
                win::FileAttributeTagInfo,
                (&raw mut ti).cast(),
                size_of::<win::FILE_ATTRIBUTE_TAG_INFO>() as DWORD,
            ) != 0
    };
    if !ok {
        return fail();
    }
    let link = ti.FileAttributes & win::FILE_ATTRIBUTE_REPARSE_POINT != 0
        && (ti.ReparseTag == win::IO_REPARSE_TAG_SYMLINK
            || ti.ReparseTag == win::IO_REPARSE_TAG_MOUNT_POINT);
    let kind = if link && follow == 0 && fd < 0 {
        3
    } else if si.Directory != 0 {
        2
    } else {
        1
    };
    let mut mode = 0o444;
    if bi.FileAttributes & win::FILE_ATTRIBUTE_READONLY == 0 {
        mode |= 0o222;
    }
    if kind == 2 {
        mode |= 0o111;
    }
    let (m, mn) = unix_time(bi.LastWriteTime);
    let (a, an) = unix_time(bi.LastAccessTime);
    let (c, _) = unix_time(bi.ChangeTime);
    let vals = [kind, si.EndOfFile, mode, m, mn, a, c, an];
    // SAFETY: the library passes a live `GcBuf<i64>` of 8 elements.
    unsafe { ptr::copy_nonoverlapping(vals.as_ptr(), out.add(HEADER_SIZE).cast(), vals.len()) };
    0
}

/// Run `f(path)` on `path` under `dir`, parked: 0 or `-error`.
fn on_path(dir: i32, path: RokStr, f: impl FnOnce(*const u16) -> win::BOOL) -> i32 {
    let mut p = match path_w(dir, path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let (ok, e) = native(|| f(p.ptr()));
    if ok == 0 { -e } else { 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_os_mkdir(dir: i32, path: RokStr, _mode: u32) -> i32 {
    // SAFETY: a NUL-terminated path.
    on_path(dir, path, |p| unsafe {
        win::CreateDirectoryW(p, ptr::null_mut())
    })
}

/// Remove a file (`is_dir == 0`; a symlink to a directory included) or
/// an empty directory.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_unlink(dir: i32, path: RokStr, is_dir: i32) -> i32 {
    // SAFETY: a NUL-terminated path.
    on_path(dir, path, |p| unsafe {
        if is_dir != 0 {
            return win::RemoveDirectoryW(p);
        }
        let ok = win::DeleteFileW(p);
        if ok != 0 || win::GetLastError() as i32 != win::ERROR_ACCESS_DENIED {
            return ok;
        }
        let a = win::GetFileAttributesW(p);
        let link_dir = win::FILE_ATTRIBUTE_DIRECTORY | win::FILE_ATTRIBUTE_REPARSE_POINT;
        if a != win::INVALID_FILE_ATTRIBUTES && a & link_dir == link_dir {
            return win::RemoveDirectoryW(p);
        }
        win::SetLastError(win::ERROR_ACCESS_DENIED as DWORD);
        0
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_os_rename(dir: i32, from: RokStr, to_dir: i32, to: RokStr) -> i32 {
    let mut b = match path_w(to_dir, to) {
        Ok(b) => b,
        Err(e) => return e,
    };
    // SAFETY: NUL-terminated paths.
    on_path(dir, from, |a| unsafe {
        win::MoveFileExW(a, b.ptr(), win::MOVEFILE_REPLACE_EXISTING)
    })
}

/// `hard == 0`: symlink `link` → `target` (`target` kept as given,
/// separators as `\`; a directory symlink if it names a directory);
/// else a hard link to `target` under `dir`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_link(dir: i32, target: RokStr, link: RokStr, hard: i32) -> i32 {
    let mut l = match path_w(dir, link) {
        Ok(l) => l,
        Err(e) => return e,
    };
    if hard != 0 {
        let mut t = match path_w(dir, target) {
            Ok(t) => t,
            Err(e) => return e,
        };
        // SAFETY: NUL-terminated paths.
        let (ok, e) = native(|| unsafe { win::CreateHardLinkW(l.ptr(), t.ptr(), ptr::null_mut()) });
        return if ok == 0 { -e } else { 0 };
    }
    let mut t = wide(target.bytes());
    for u in t.as_mut_slice() {
        if *u == u16::from(b'/') {
            *u = u16::from(b'\\');
        }
    }
    // Where `target` points, to tell a directory link.
    let t_bytes = target.bytes();
    let absolute =
        t_bytes.starts_with(b"/") || t_bytes.starts_with(b"\\") || t_bytes.get(1) == Some(&b':');
    let mut at = Wide::new();
    if !absolute {
        let ls = l.as_slice();
        let cut = ls
            .iter()
            .rposition(|&u| u == u16::from(b'\\') || u == u16::from(b'/'));
        if let Some(i) = cut {
            at.extend(&ls[..=i]);
        }
    }
    at.extend(t.as_slice());
    // SAFETY: NUL-terminated paths.
    let (ok, e) = native(|| unsafe {
        let a = win::GetFileAttributesW(at.ptr());
        let mut flags = win::SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE;
        if a != win::INVALID_FILE_ATTRIBUTES && a & win::FILE_ATTRIBUTE_DIRECTORY != 0 {
            flags |= win::SYMBOLIC_LINK_FLAG_DIRECTORY;
        }
        win::CreateSymbolicLinkW(l.ptr(), t.ptr(), flags)
    });
    if ok == 0 { -e } else { 0 }
}

/// A directory listing in progress (`rok_os_opendir`'s handle).
struct Listing {
    find: HANDLE,
    data: win::WIN32_FIND_DATAW,
    /// `data` holds an entry not handed out yet.
    pending: bool,
}

/// Open `path` under `dir` for listing: a listing (its address, zero
/// extended: i686's may have the top bit set) or `-error`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_opendir(dir: i32, path: RokStr) -> i64 {
    let mut p = match path_w(dir, path) {
        Ok(p) => p,
        Err(e) => return i64::from(e),
    };
    if !p
        .as_slice()
        .last()
        .is_some_and(|&u| u == u16::from(b'\\') || u == u16::from(b'/'))
    {
        p.push(u16::from(b'\\'));
    }
    p.push(u16::from(b'*'));
    // SAFETY: a malloc'd listing, freed by `rok_os_closedir` (or here).
    unsafe {
        let l = libc::malloc(size_of::<Listing>()).cast::<Listing>();
        if l.is_null() {
            rokr_common::oom();
        }
        let (find, e) = native(|| win::FindFirstFileW(p.ptr(), &raw mut (*l).data));
        if find == win::INVALID_HANDLE_VALUE {
            libc::free(l.cast());
            return -i64::from(e);
        }
        (*l).find = find;
        (*l).pending = true;
        l as usize as i64
    }
}

/// The next entry name (skipping `.` / `..`) into `buf[off..off + len]`:
/// its length, 0 at the end, or `-error`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_readdir(d: isize, buf: *mut u8, off: usize, len: usize) -> isize {
    let l = d as *mut Listing;
    loop {
        // SAFETY: `d` is a live listing from `rok_os_opendir`.
        unsafe {
            if !(*l).pending && win::FindNextFileW((*l).find, &raw mut (*l).data) == 0 {
                let e = errno();
                return if e == win::ERROR_NO_MORE_FILES {
                    0
                } else {
                    -(e as isize)
                };
            }
            (*l).pending = false;
            let name = &(*l).data.cFileName;
            let n = name.iter().position(|&u| u == 0).unwrap_or(name.len());
            let name = narrow(&name[..n]);
            let name = name.as_slice();
            if name == b"." || name == b".." {
                continue;
            }
            if name.len() > len {
                return -(win::ERROR_FILENAME_EXCED_RANGE as isize);
            }
            return put(buf, off, len, name);
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_os_closedir(d: isize) {
    let l = d as *mut Listing;
    // SAFETY: a live listing from `rok_os_opendir`, closed once.
    unsafe {
        win::FindClose((*l).find);
        libc::free(l.cast());
    }
}

/// The working directory into `buf[off..off + len]`: its full length or
/// `-error`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_getcwd(buf: *mut u8, off: usize, len: usize) -> isize {
    // SAFETY: `b` is writable for `n` units.
    match fill_wide(|b, n| unsafe { win::GetCurrentDirectoryW(n, b) }) {
        Ok(w) => put_wide(buf, off, len, w.as_slice()),
        Err(e) => -(e as isize),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_os_chdir(path: RokStr) -> i32 {
    // SAFETY: a NUL-terminated path.
    on_path(-1, path, |p| unsafe { win::SetCurrentDirectoryW(p) })
}

/// The absolute, link-resolved form of `path` into `buf[off..off +
/// len]` (`C:\..` / `\\server\..`, no `\\?\`): its full length or
/// `-error`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_realpath(path: RokStr, buf: *mut u8, off: usize, len: usize) -> isize {
    let mut p = wide(path.bytes());
    let r = native(|| open_meta(&mut p, true).and_then(|o| final_path(o.0))).0;
    match r {
        Ok(w) => put_wide(buf, off, len, friendly(w.as_slice()).as_slice()),
        Err(e) => e as isize,
    }
}

/// Absolute path of the running executable into `buf[off..off + len]`:
/// its full length or `-error`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_current_exe(buf: *mut u8, off: usize, len: usize) -> isize {
    // SAFETY: `b` is writable for `n` units.
    match fill_wide(|b, n| unsafe { win::GetModuleFileNameW(0, b, n) }) {
        Ok(w) => put_wide(buf, off, len, w.as_slice()),
        Err(e) => -(e as isize),
    }
}

// Arguments and environment

fn args() -> &'static Args {
    static ARGS: AtomicPtr<Args> = AtomicPtr::new(ptr::null_mut());
    let a = ARGS.load(Ordering::Acquire);
    if !a.is_null() {
        // SAFETY: published once, never freed.
        return unsafe { &*a };
    }
    // SAFETY: the process command line lives as long as the process.
    let line = unsafe { sys::wide_cstr(win::GetCommandLineW()) };
    let parsed = parse_command_line(line);
    // SAFETY: plain allocation, never freed.
    let p = unsafe { libc::malloc(size_of::<Args>()) }.cast::<Args>();
    if p.is_null() {
        rokr_common::oom();
    }
    // SAFETY: fresh allocation.
    unsafe { p.write(parsed) };
    match ARGS.compare_exchange(ptr::null_mut(), p, Ordering::AcqRel, Ordering::Acquire) {
        // SAFETY: just published.
        Ok(_) => unsafe { &*p },
        Err(won) => {
            // SAFETY: ours, never published; `won` lives forever.
            unsafe {
                ptr::drop_in_place(p);
                libc::free(p.cast());
                &*won
            }
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_os_argc() -> usize {
    args().count
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_os_arg(i: usize) -> RokStr {
    let a = args();
    a.bytes
        .as_slice()
        .split(|&b| b == 0)
        .nth(i)
        .filter(|_| i < a.count)
        .map_or_else(|| build(&[]), |s| build(&[s]))
}

/// Entry `i` of the environment block (`"KEY=VALUE"`), `""` past the end.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_env_at(i: usize) -> RokStr {
    // SAFETY: a double-NUL-terminated block, freed below.
    unsafe {
        let env = win::GetEnvironmentStringsW();
        if env.is_null() {
            return build(&[]);
        }
        let mut p = env.cast_const();
        let mut k = 0;
        let mut found = MBuf::new();
        while *p != 0 {
            let entry = sys::wide_cstr(p);
            if k == i {
                found = narrow(entry);
                break;
            }
            p = p.add(entry.len() + 1);
            k += 1;
        }
        win::FreeEnvironmentStringsW(env);
        build(&[found.as_slice()])
    }
}

/// Variable `name` into `buf[off..off + len]`: its full length, or
/// `-ERROR_ENVVAR_NOT_FOUND` when unset.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_getenv(name: RokStr, buf: *mut u8, off: usize, len: usize) -> isize {
    let mut n = wide(name.bytes());
    let np = n.ptr();
    // SAFETY: `b` is writable for `k` units; `np` is NUL-terminated.
    match fill_wide(|b, k| unsafe { win::GetEnvironmentVariableW(np, b, k) }) {
        Ok(w) => put_wide(buf, off, len, w.as_slice()),
        Err(e) => -(e as isize),
    }
}

/// Set (`value` given) or remove a variable. 0 or `-error`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_setenv(name: RokStr, value: RokStr, unset: i32) -> i32 {
    let mut n = wide(name.bytes());
    let mut v = wide(value.bytes());
    // SAFETY: NUL-terminated strings (or null to remove).
    let ok = unsafe {
        win::SetEnvironmentVariableW(n.ptr(), if unset != 0 { ptr::null() } else { v.ptr() })
    };
    if ok == 0 { fail() } else { 0 }
}

// Processes

/// Children not reaped yet: `(pid, process handle)`.
struct Children {
    lock: sys::Lock,
    list: UnsafeCell<MBuf<(u32, HANDLE)>>,
}

// SAFETY: `list` is only touched with `lock` held.
unsafe impl Sync for Children {}

static CHILDREN: Children = Children {
    lock: sys::Lock::new(),
    list: UnsafeCell::new(MBuf::new()),
};

/// With the children lock held: `f` on the list.
fn children<R>(f: impl FnOnce(&mut MBuf<(u32, HANDLE)>) -> R) -> R {
    CHILDREN.lock.lock();
    // SAFETY: the lock is held.
    let r = f(unsafe { &mut *CHILDREN.list.get() });
    CHILDREN.lock.unlock();
    r
}

/// The handle of child `pid`, if it has not been reaped.
fn child(pid: u32) -> Option<HANDLE> {
    children(|l| l.as_slice().iter().find(|c| c.0 == pid).map(|c| c.1))
}

/// The bytes of `s` for the length of a call (the caller roots `s`).
///
/// # Safety
/// The slice must not outlive `s`.
const unsafe fn statics(s: RokStr) -> &'static [u8] {
    if s.len == 0 {
        return &[];
    }
    // SAFETY: caller contract.
    unsafe { core::slice::from_raw_parts(s.ptr, s.len) }
}

/// `name` with `ext` (ASCII, lower case) as its extension, any case.
fn has_ext(name: &[u8], ext: &[u8]) -> bool {
    name.len() >= ext.len() && name[name.len() - ext.len()..].eq_ignore_ascii_case(ext)
}

/// The program to start: `prog` itself when it holds a separator or a
/// drive, else the first `<dir>\prog` (or `prog.exe` without an
/// extension) that exists along `PATH`.
fn find_program(prog: &[u8]) -> Result<Wide, i32> {
    if prog.iter().any(|&b| b == b'/' || b == b'\\' || b == b':') {
        return Ok(wide(prog));
    }
    let name = ascii_w(b"PATH\0");
    // SAFETY: `b` is writable for `n` units; `name` is NUL-terminated.
    let path = fill_wide(|b, n| unsafe { win::GetEnvironmentVariableW(name.as_ptr(), b, n) })
        .unwrap_or_else(|_| Wide::new());
    let has_dot = prog.contains(&b'.');
    for dir in path.as_slice().split(|&u| u == u16::from(b';')) {
        if dir.is_empty() {
            continue;
        }
        for ext in [&b""[..], b".exe"] {
            if has_dot && !ext.is_empty() {
                continue;
            }
            let mut c = Wide::new();
            c.extend(dir);
            if !dir.ends_with(&[u16::from(b'\\')]) {
                c.push(u16::from(b'\\'));
            }
            push_wide(&mut c, prog);
            push_wide(&mut c, ext);
            // SAFETY: a NUL-terminated path.
            let a = unsafe { win::GetFileAttributesW(c.ptr()) };
            if a != win::INVALID_FILE_ATTRIBUTES && a & win::FILE_ATTRIBUTE_DIRECTORY == 0 {
                return Ok(c);
            }
        }
    }
    Err(-win::ERROR_FILE_NOT_FOUND)
}

/// An inheritable copy of `hd` (0 when `hd` is 0 / invalid).
fn inheritable(hd: HANDLE) -> Result<HANDLE, i32> {
    if hd == 0 || hd == win::INVALID_HANDLE_VALUE {
        return Ok(0);
    }
    let mut out = 0;
    // SAFETY: duplicates within this process.
    let ok = unsafe {
        let me = win::GetCurrentProcess();
        win::DuplicateHandle(me, hd, me, &mut out, 0, 1, win::DUPLICATE_SAME_ACCESS)
    };
    if ok == 0 { Err(fail()) } else { Ok(out) }
}

/// `CreateProcessW`. `argv` / `envs` hold `argc` / `envc` NUL-terminated
/// pieces (`envc < 0`: inherit the environment); `cwd` empty = inherit;
/// `pgid` 0 starts a new process group (other values: unsupported).
/// Stdio modes: `-1` inherit, `-2` null, `-3` piped, `>= 0` that fd. The
/// pipe ends (or -1) go to `out[0..3]` (stdin write end, stdout / stderr
/// read ends). Returns the pid or `-error`.
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
    for i in 0..3 {
        // SAFETY: the library passes a live `GcBuf<i32>` of 3 elements.
        unsafe { *cell(out, i) = -1 };
    }
    if pgid > 0 {
        return -win::ERROR_NOT_SUPPORTED;
    }
    let pieces = |s: &'static [u8], n: usize| s.split(|&b| b == 0).take(n);
    // SAFETY: the library keeps the blobs alive across the call.
    let (argv, envs) = unsafe { (statics(argv), statics(envs)) };
    let Some(prog) = pieces(argv, argc).next() else {
        return -win::ERROR_INVALID_PARAMETER;
    };
    // `cmd.exe` re-parses a batch file's command line: refuse what it
    // would expand or split on.
    if (has_ext(prog, b".bat") || has_ext(prog, b".cmd"))
        && pieces(argv, argc)
            .skip(1)
            .any(|a| a.iter().any(|b| matches!(b, b'"' | b'%' | b'\r' | b'\n')))
    {
        return -win::ERROR_INVALID_PARAMETER;
    }
    let mut app = match find_program(prog) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let mut cmd = Wide::new();
    for a in pieces(argv, argc) {
        push_arg(&mut cmd, wide(a).as_slice());
    }
    let mut env = Wide::new();
    if envc >= 0 {
        for kv in pieces(envs, envc as usize) {
            push_wide(&mut env, kv);
            env.push(0);
        }
        if envc == 0 {
            env.push(0);
        }
        env.push(0);
    }
    let mut dir = wide(cwd.bytes());

    // [child end, parent end] per stream; the child ends are inheritable.
    let mut ends = [[0 as HANDLE; 2]; 3];
    let r = setup_stdio([stdio_in, stdio_out, stdio_err], &mut ends);
    let r = r.and_then(|()| {
        // SAFETY: the buffers above outlive the call; the attribute list
        // is sized by the first call.
        unsafe {
            create(
                &mut app,
                &mut cmd,
                &mut env,
                envc >= 0,
                &mut dir,
                cwd.len > 0,
                pgid == 0,
                &ends,
            )
        }
    });
    for (i, e) in ends.iter().enumerate() {
        // SAFETY: our own handles.
        unsafe {
            if e[0] != 0 {
                win::CloseHandle(e[0]);
            }
            if r.is_err() && e[1] != 0 {
                win::CloseHandle(e[1]);
            }
            if r.is_ok() && e[1] != 0 {
                *cell(out, i) = e[1] as i32;
            }
        }
    }
    match r {
        Ok(pid) => pid as i32,
        Err(e) => e,
    }
}

/// The child's standard handles into `ends` (see [`rok_os_spawn`]).
fn setup_stdio(modes: [i32; 3], ends: &mut [[HANDLE; 2]; 3]) -> Result<(), i32> {
    const STD: [DWORD; 3] = [
        win::STD_INPUT_HANDLE,
        win::STD_OUTPUT_HANDLE,
        win::STD_ERROR_HANDLE,
    ];
    for (i, &m) in modes.iter().enumerate() {
        ends[i][0] = match m {
            // SAFETY: plain query.
            -1 => inheritable(unsafe { win::GetStdHandle(STD[i]) })?,
            -2 => {
                let access = if i == 0 {
                    win::GENERIC_READ
                } else {
                    win::GENERIC_WRITE
                };
                let mut sa = win::SECURITY_ATTRIBUTES {
                    nLength: size_of::<win::SECURITY_ATTRIBUTES>() as DWORD,
                    lpSecurityDescriptor: ptr::null_mut(),
                    bInheritHandle: 1,
                };
                let nul = [u16::from(b'N'), u16::from(b'U'), u16::from(b'L'), 0];
                // SAFETY: a NUL-terminated name.
                let hd = unsafe {
                    win::CreateFileW(
                        nul.as_ptr(),
                        access,
                        win::FILE_SHARE_READ | win::FILE_SHARE_WRITE,
                        &mut sa,
                        win::OPEN_EXISTING,
                        0,
                        0,
                    )
                };
                if hd == win::INVALID_HANDLE_VALUE {
                    return Err(fail());
                }
                hd
            }
            -3 => {
                let (child, parent) = if i == 0 {
                    let (mut r, mut w) = (0, 0);
                    // SAFETY: valid out-pointers; neither end is inheritable.
                    if unsafe { win::CreatePipe(&mut r, &mut w, ptr::null_mut(), 0) } == 0 {
                        return Err(fail());
                    }
                    (r, w)
                } else {
                    let (ours, theirs) = output_pipe()?;
                    (theirs, ours)
                };
                ends[i][1] = parent;
                let c = inheritable(child);
                // SAFETY: ours; `c` is the inheritable copy.
                unsafe { win::CloseHandle(child) };
                c?
            }
            fd => inheritable(h(fd))?,
        };
    }
    Ok(())
}

/// `CreateProcessW` with exactly the `ends` child handles inherited.
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
unsafe fn create(
    app: &mut Wide,
    cmd: &mut Wide,
    env: &mut Wide,
    own_env: bool,
    dir: &mut Wide,
    own_dir: bool,
    new_group: bool,
    ends: &[[HANDLE; 2]; 3],
) -> Result<u32, i32> {
    let mut list = [0 as HANDLE; 3];
    let mut n = 0;
    for e in ends {
        if e[0] != 0 && !list[..n].contains(&e[0]) {
            list[n] = e[0];
            n += 1;
        }
    }
    // SAFETY: the attribute list is sized by the first call, built in a
    // malloc block, deleted and freed before returning.
    unsafe {
        let mut size = 0;
        win::InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size);
        let attrs = libc::malloc(size);
        if attrs.is_null() {
            rokr_common::oom();
        }
        let r = if win::InitializeProcThreadAttributeList(attrs, 1, 0, &mut size) == 0 {
            Err(fail())
        } else {
            let r = if n == 0
                || win::UpdateProcThreadAttribute(
                    attrs,
                    0,
                    win::PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                    list.as_ptr().cast(),
                    n * size_of::<HANDLE>(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                ) != 0
            {
                let mut si: win::STARTUPINFOEXW = zeroed();
                si.StartupInfo.cb = size_of::<win::STARTUPINFOEXW>() as DWORD;
                si.StartupInfo.dwFlags = win::STARTF_USESTDHANDLES;
                si.StartupInfo.hStdInput = ends[0][0];
                si.StartupInfo.hStdOutput = ends[1][0];
                si.StartupInfo.hStdError = ends[2][0];
                si.lpAttributeList = if n == 0 { ptr::null_mut() } else { attrs };
                let mut flags = win::CREATE_UNICODE_ENVIRONMENT | win::EXTENDED_STARTUPINFO_PRESENT;
                if new_group {
                    flags |= win::CREATE_NEW_PROCESS_GROUP;
                }
                let mut pi: win::PROCESS_INFORMATION = zeroed();
                let cmd_p = cmd.ptr().cast_mut();
                let env_p = if own_env {
                    env.ptr().cast_mut().cast()
                } else {
                    ptr::null_mut()
                };
                let dir_p = if own_dir { dir.ptr() } else { ptr::null() };
                let app_p = app.ptr();
                let (ok, e) = native(|| {
                    win::CreateProcessW(
                        app_p,
                        cmd_p,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        i32::from(n > 0),
                        flags,
                        env_p,
                        dir_p,
                        (&raw mut si).cast(),
                        &mut pi,
                    )
                });
                if ok == 0 {
                    Err(-e)
                } else {
                    win::CloseHandle(pi.hThread);
                    children(|l| l.push((pi.dwProcessId, pi.hProcess)));
                    Ok(pi.dwProcessId)
                }
            } else {
                Err(fail())
            };
            win::DeleteProcThreadAttributeList(attrs);
            r
        };
        libc::free(attrs);
        r
    }
}

/// Wait for child `pid`: the pid (its exit code in `out[0]`), 0 if
/// `nohang` and still running, or `-error`. `nohang` bit 0: do not
/// block; bit 1: only wait for the end, keep the handle (`out`
/// untouched).
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_waitpid(pid: i32, nohang: i32, out: *mut u8) -> i32 {
    let Some(hd) = child(pid as u32) else {
        return -win::ERROR_INVALID_PARAMETER;
    };
    let ms = if nohang & 1 != 0 { 0 } else { win::INFINITE };
    // SAFETY: a live process handle (only this call closes it).
    let (w, e) = native(|| unsafe { win::WaitForSingleObject(hd, ms) });
    if w == win::WAIT_TIMEOUT {
        return 0;
    }
    if w != win::WAIT_OBJECT_0 {
        return -e;
    }
    if nohang & 2 != 0 {
        return pid;
    }
    let mut code = 0;
    // SAFETY: as above.
    if unsafe { win::GetExitCodeProcess(hd, &mut code) } == 0 {
        return fail();
    }
    children(|l| {
        if let Some(i) = l.as_slice().iter().position(|c| c.0 == pid as u32) {
            let last = l.len() - 1;
            l.as_mut_slice().swap(i, last);
            l.truncate(last);
            // SAFETY: the handle leaves the list here, once.
            unsafe { win::CloseHandle(hd) };
        }
    });
    // SAFETY: a live one-element `GcBuf<i32>`.
    unsafe { *cell(out, 0) = code as i32 };
    pid
}

/// Console control events routed to the signal pipe: bit `sig` set.
static SIGNALS: AtomicU32 = AtomicU32::new(0);
/// Write end of the signal pipe (`rok_os_signal_install`), or 0.
static SIGNAL_PIPE: AtomicUsize = AtomicUsize::new(0);

/// The signal number of console event `ev`.
const fn event_signal(ev: DWORD) -> i32 {
    match ev {
        win::CTRL_C_EVENT => 2,
        win::CTRL_BREAK_EVENT => 21,
        win::CTRL_CLOSE_EVENT | win::CTRL_LOGOFF_EVENT | win::CTRL_SHUTDOWN_EVENT => 1,
        _ => 0,
    }
}

/// Send `sig` down the signal pipe if it is routed there.
fn deliver(sig: i32) -> bool {
    let pipe = SIGNAL_PIPE.load(Ordering::Acquire) as HANDLE;
    if pipe == 0 || sig <= 0 || SIGNALS.load(Ordering::Acquire) & (1 << sig) == 0 {
        return false;
    }
    let b = sig as u8;
    let mut n = 0;
    // SAFETY: one byte from a local to our pipe.
    unsafe { win::WriteFile(pipe, (&raw const b).cast(), 1, &mut n, ptr::null_mut()) };
    true
}

/// The console control handler (on a thread the system starts): the
/// event's signal down the pipe when routed, else the default action.
/// After a close / logoff / shutdown event the system ends the process
/// once handlers return, so the rok handler may not get to run.
unsafe extern "system" fn on_ctrl(ev: DWORD) -> win::BOOL {
    i32::from(deliver(event_signal(ev)))
}

/// Route `sig` (2 `SIGINT`: Ctrl-C, 21 `SIGBREAK`: Ctrl-Break, 1
/// `SIGHUP`: console close / logoff / shutdown) to the signal pipe. The
/// first call creates the pipe; `*read_end` gets its read end on that
/// call, -1 after. 0 or `-error` (`ERROR_NOT_SUPPORTED` for other
/// numbers).
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_signal_install(sig: i32, read_end: *mut u8) -> i32 {
    // SAFETY: the library passes a live `GcBuf<i32>` of 1 element.
    unsafe { *cell(read_end, 0) = -1 };
    if !matches!(sig, 1 | 2 | 21) {
        return -win::ERROR_NOT_SUPPORTED;
    }
    if SIGNAL_PIPE.load(Ordering::Acquire) == 0 {
        let (mut r, mut w) = (0, 0);
        // SAFETY: valid out-pointers; the handler is a plain function.
        unsafe {
            if win::CreatePipe(&mut r, &mut w, ptr::null_mut(), 0) == 0 {
                return fail();
            }
            if win::SetConsoleCtrlHandler(Some(on_ctrl), 1) == 0 {
                let e = fail();
                win::CloseHandle(r);
                win::CloseHandle(w);
                return e;
            }
            *cell(read_end, 0) = r as i32;
        }
        SIGNAL_PIPE.store(w as usize, Ordering::Release);
    }
    SIGNALS.fetch_or(1 << sig, Ordering::AcqRel);
    0
}

/// `sig` to this process: down the signal pipe when routed, else the
/// CRT's `raise` (its default action ends the process).
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_raise(sig: i32) -> i32 {
    if deliver(sig) {
        return 0;
    }
    // SAFETY: plain CRT call.
    if unsafe { libc::raise(sig) } != 0 {
        -win::ERROR_INVALID_PARAMETER
    } else {
        0
    }
}

/// End process `pid` (`TerminateProcess`, exit code 1) for any `sig`
/// but 0, which only checks that the process exists.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_kill(pid: i32, sig: i32) -> i32 {
    let mine = child(pid as u32);
    let hd = match mine {
        Some(hd) => hd,
        // SAFETY: plain call; closed below.
        None => unsafe {
            win::OpenProcess(
                win::PROCESS_TERMINATE | win::PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid as DWORD,
            )
        },
    };
    if hd == 0 {
        return fail();
    }
    // SAFETY: a live process handle.
    let r = if sig == 0 || unsafe { win::TerminateProcess(hd, 1) } != 0 {
        0
    } else {
        fail()
    };
    if mine.is_none() {
        // SAFETY: ours, from `OpenProcess`.
        unsafe { win::CloseHandle(hd) };
    }
    r
}

/// No pidfds on Windows: `-ERROR_NOT_SUPPORTED` (`Child` falls back to
/// its pid).
#[unsafe(no_mangle)]
pub const extern "C" fn rok_os_pidfd_open(_pid: i32) -> i32 {
    -win::ERROR_NOT_SUPPORTED
}

#[unsafe(no_mangle)]
pub const extern "C" fn rok_os_pidfd_signal(_fd: i32, _sig: i32) -> i32 {
    -win::ERROR_NOT_SUPPORTED
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_os_getpid() -> u32 {
    // SAFETY: no preconditions.
    unsafe { win::GetCurrentProcessId() }
}

/// The parent's pid (0 if unknown).
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_getppid() -> u32 {
    // SAFETY: zeroed info is a valid out-buffer; class 0 is
    // `ProcessBasicInformation`.
    unsafe {
        let mut pbi: win::PROCESS_BASIC_INFORMATION = zeroed();
        let s = win::NtQueryInformationProcess(
            win::GetCurrentProcess(),
            0,
            (&raw mut pbi).cast(),
            size_of::<win::PROCESS_BASIC_INFORMATION>() as u32,
            ptr::null_mut(),
        );
        if s < 0 {
            0
        } else {
            pbi.InheritedFromUniqueProcessId as u32
        }
    }
}

/// Abnormal termination, no cleanup.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_abort() -> ! {
    // SAFETY: never returns.
    unsafe { libc::abort() }
}

// Randomness and trust anchors (crypto, net::tls)

/// Fill `buf[off..off + len]` from the system RNG (`BCryptGenRandom`): 0
/// or `-error`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_random(buf: *mut u8, off: usize, len: usize) -> i32 {
    let mut done = 0;
    while done < len {
        let n = (len - done).min(1 << 30);
        // SAFETY: the library passes a live region of `len` bytes.
        let s = unsafe {
            win::BCryptGenRandom(
                ptr::null_mut(),
                region(buf, off + done),
                n as u32,
                win::BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        if s < 0 {
            return -win::ERROR_NOT_SUPPORTED;
        }
        done += n;
    }
    0
}

/// `CERT_CONTEXT` (wincrypt.h).
#[repr(C)]
struct CertContext {
    encoding: DWORD,
    der: *const u8,
    der_len: DWORD,
    info: *mut c_void,
    store: *mut c_void,
}

/// The `ROOT` system store's certificates (crypt32, loaded on first use;
/// it is not in the import set) as records `[u32 big-endian length][DER]`.
fn root_store() -> Option<MBuf<u8>> {
    type Open = unsafe extern "system" fn(usize, *const u16) -> *mut c_void;
    type Enum = unsafe extern "system" fn(*mut c_void, *const CertContext) -> *const CertContext;
    type Close = unsafe extern "system" fn(*mut c_void, DWORD) -> win::BOOL;
    // SAFETY: plain module load; each export has the declared signature.
    unsafe {
        let m = win::LoadLibraryW(wide(b"crypt32.dll").ptr());
        if m == 0 {
            return None;
        }
        let proc = |name: &[u8]| {
            let p = win::GetProcAddress(m, name.as_ptr());
            (!p.is_null()).then_some(p)
        };
        let open: Open = core::mem::transmute(proc(b"CertOpenSystemStoreW\0")?);
        let next: Enum = core::mem::transmute(proc(b"CertEnumCertificatesInStore\0")?);
        let close: Close = core::mem::transmute(proc(b"CertCloseStore\0")?);
        let store = open(0, wide(b"ROOT").ptr());
        if store.is_null() {
            return None;
        }
        let mut out = MBuf::new();
        let mut c: *const CertContext = ptr::null();
        loop {
            c = next(store, c);
            if c.is_null() {
                break;
            }
            let n = (*c).der_len as usize;
            out.extend(&(n as u32).to_be_bytes());
            out.extend(core::slice::from_raw_parts((*c).der, n));
        }
        close(store, 0);
        Some(out)
    }
}

/// The OS's trusted root certificates into `buf[off..off + len]` as
/// records `[u32 big-endian length][DER]`: their full length or `-error`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_os_trust_roots(buf: *mut u8, off: usize, len: usize) -> isize {
    let (roots, _) = native(root_store);
    match roots {
        Some(v) => put(buf, off, len, v.as_slice()),
        None => -(win::ERROR_NOT_SUPPORTED as isize),
    }
}
