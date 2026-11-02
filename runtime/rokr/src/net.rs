//! Sockets behind `rok::std::net` and the readiness poller
//! of the I/O driver: kqueue on BSD / macOS, epoll on Linux, `poll`
//! (`poll_oneoff` / `wasi:io/poll`) on WASI. No ABI
//! stability. Conventions as in `os`: results are `>= 0` or
//! `-errno`; byte data is a `GcBuf<u8>` region.
//!
//! WASI preview 2 has the BSD API through wasi-libc. Preview 1
//! (`wasm32-wasip1-threads`, the browser) only uses sockets the host hands
//! in ([`rok_net_listeners`]); the rest fails with `ENOSYS`.
//!
//! Sockets are non-blocking; blocking calls wait in [`rok_net_wait`].
//!
//! Addresses cross as [`ADDR_WORDS`] words of a `GcBuf<i64>`:
//! `[family (4 / 6), port, flow_info, scope_id, byte0 .. byte15]`.

#[cfg(any(unix, target_env = "p2"))]
use core::mem::{size_of, zeroed};
#[cfg(target_os = "linux")]
use core::ptr;
#[cfg(not(target_os = "wasi"))]
use core::sync::atomic::{AtomicI32, Ordering};

use rokr_common::object_header::HEADER_SIZE;

#[cfg(any(unix, target_env = "p2"))]
use crate::os::{EAI_BASE, cs};
use crate::os::{errno, native, region, ret};
#[cfg(any(unix, target_env = "p2"))]
use crate::str::RokStr;

const ADDR_WORDS: usize = 20;

/// Word `i` of the `GcBuf<i64>` `buf`.
///
/// # Safety
/// `buf` must be a live `GcBuf<i64>` with more than `i` elements.
#[allow(clippy::cast_ptr_alignment)] // GC payloads are 8-aligned
const unsafe fn word(buf: *mut u8, i: usize) -> *mut i64 {
    // SAFETY: caller contract.
    unsafe { buf.add(HEADER_SIZE).cast::<i64>().add(i) }
}

#[cfg(any(unix, target_env = "p2"))]
/// The address in `buf` as a `sockaddr_storage` and its length.
fn read_addr(buf: *mut u8) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: the library passes a live `ADDR_WORDS` buffer; the storage
    // is large enough for either family.
    unsafe {
        let w = |i| *word(buf, i);
        let mut ss: libc::sockaddr_storage = zeroed();
        let port = (w(1) as u16).to_be();
        if w(0) == 6 {
            let a = (&raw mut ss).cast::<libc::sockaddr_in6>();
            (*a).sin6_family = libc::AF_INET6 as libc::sa_family_t;
            (*a).sin6_port = port;
            (*a).sin6_flowinfo = w(2) as u32;
            (*a).sin6_scope_id = w(3) as u32;
            for i in 0..16 {
                (*a).sin6_addr.s6_addr[i] = w(4 + i) as u8;
            }
            set_len(&raw mut ss, size_of::<libc::sockaddr_in6>());
            (ss, size_of::<libc::sockaddr_in6>() as libc::socklen_t)
        } else {
            let a = (&raw mut ss).cast::<libc::sockaddr_in>();
            (*a).sin_family = libc::AF_INET as libc::sa_family_t;
            (*a).sin_port = port;
            let b = [w(4) as u8, w(5) as u8, w(6) as u8, w(7) as u8];
            (*a).sin_addr.s_addr = u32::from_ne_bytes(b);
            set_len(&raw mut ss, size_of::<libc::sockaddr_in>());
            (ss, size_of::<libc::sockaddr_in>() as libc::socklen_t)
        }
    }
}

#[cfg(any(unix, target_env = "p2"))]
/// BSD sockaddrs carry their length.
#[allow(clippy::missing_const_for_fn)]
unsafe fn set_len(ss: *mut libc::sockaddr_storage, n: usize) {
    #[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
    // SAFETY: caller passes a valid storage.
    unsafe {
        (*ss).ss_len = n as u8;
    }
    let _ = (ss, n);
}

#[cfg(any(unix, target_env = "p2"))]
/// Store the address at `sa` into `buf` (no-op for a null `buf`).
#[allow(clippy::cast_ptr_alignment)] // a sockaddr of its own family
fn write_addr(buf: *mut u8, sa: *const libc::sockaddr) {
    if buf.is_null() {
        return;
    }
    // SAFETY: `sa` points at a sockaddr of its own family; `buf` is a
    // live `ADDR_WORDS` buffer.
    unsafe {
        let put = |i, v: i64| *word(buf, i) = v;
        for i in 0..ADDR_WORDS {
            put(i, 0);
        }
        if i32::from((*sa).sa_family) == libc::AF_INET6 {
            let a = sa.cast::<libc::sockaddr_in6>();
            put(0, 6);
            put(1, i64::from(u16::from_be((*a).sin6_port)));
            put(2, i64::from((*a).sin6_flowinfo));
            put(3, i64::from((*a).sin6_scope_id));
            for i in 0..16 {
                put(4 + i, i64::from((*a).sin6_addr.s6_addr[i]));
            }
        } else {
            let a = sa.cast::<libc::sockaddr_in>();
            put(0, 4);
            put(1, i64::from(u16::from_be((*a).sin_port)));
            for (i, b) in (*a).sin_addr.s_addr.to_ne_bytes().iter().enumerate() {
                put(4 + i, i64::from(*b));
            }
        }
    }
}

/// Non-blocking + close-on-exec.
fn prepare(fd: i32) -> i32 {
    // SAFETY: plain fcntl calls on an fd we own.
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl < 0 || libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) < 0 {
            return -errno();
        }
        #[cfg(unix)]
        if libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
            return -errno();
        }
        #[cfg(target_vendor = "apple")]
        {
            let on: libc::c_int = 1;
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&raw const on).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
    0
}

/// `send` flags: no SIGPIPE where the flag exists.
#[cfg(not(any(target_vendor = "apple", target_os = "wasi")))]
const SEND_FLAGS: libc::c_int = libc::MSG_NOSIGNAL;
#[cfg(any(target_vendor = "apple", target_os = "wasi"))]
const SEND_FLAGS: libc::c_int = 0;

#[cfg(any(unix, target_env = "p2"))]
/// A new non-blocking socket: `family` 4 / 6, `kind` 1 stream / 2
/// datagram / 3 raw, `proto` an IP protocol number (0: the default for
/// `kind`). The fd or `-errno`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_socket(family: i32, kind: i32, proto: i32) -> i32 {
    let af = if family == 6 {
        libc::AF_INET6
    } else {
        libc::AF_INET
    };
    let ty = match kind {
        2 => libc::SOCK_DGRAM,
        #[cfg(unix)]
        3 => libc::SOCK_RAW,
        _ => libc::SOCK_STREAM,
    };
    // wasi-libc's preview 2 sockets take no `fcntl`: non-blocking from
    // the start.
    #[cfg(target_env = "p2")]
    let ty = ty | libc::SOCK_NONBLOCK;
    // SAFETY: plain libc call.
    let fd = unsafe { libc::socket(af, ty, proto) };
    if fd < 0 {
        return -errno();
    }
    #[cfg(unix)]
    {
        let r = prepare(fd);
        if r < 0 {
            // SAFETY: our fd.
            unsafe { libc::close(fd) };
            return r;
        }
    }
    fd
}

#[cfg(any(unix, target_env = "p2"))]
/// `bind`, nothing else (a listener sets `SO_REUSEADDR` first itself).
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_bind(fd: i32, addr: *mut u8) -> i32 {
    let (ss, len) = read_addr(addr);
    // SAFETY: a valid sockaddr.
    ret(unsafe { libc::bind(fd, (&raw const ss).cast(), len) } as isize) as i32
}

#[cfg(any(unix, target_env = "p2"))]
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_listen(fd: i32, backlog: i32) -> i32 {
    // SAFETY: plain libc call.
    ret(unsafe { libc::listen(fd, backlog) } as isize) as i32
}

#[cfg(any(unix, target_env = "p2"))]
/// Start a connect: 0 when connected, 1 while pending (wait for
/// writability, then [`rok_net_sock_error`]), or `-errno`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_connect(fd: i32, addr: *mut u8) -> i32 {
    let (ss, len) = read_addr(addr);
    loop {
        // SAFETY: valid sockaddr.
        let r = unsafe { libc::connect(fd, (&raw const ss).cast(), len) };
        if r == 0 {
            return 0;
        }
        match errno() {
            libc::EINTR => {}
            libc::EINPROGRESS => return 1,
            e => return -e,
        }
    }
}

#[cfg(any(unix, target_env = "p2"))]
/// The pending error of `fd` (`SO_ERROR`): 0 or `-errno`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_sock_error(fd: i32) -> i32 {
    let mut e: libc::c_int = 0;
    let mut n = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: valid out-pointers.
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            (&raw mut e).cast(),
            &raw mut n,
        )
    };
    if r < 0 { -errno() } else { -e }
}

#[cfg(any(unix, target_env = "p2"))]
/// Accept one connection (non-blocking: `-EAGAIN` if none is queued);
/// its peer goes to `out`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_accept(fd: i32, out: *mut u8) -> i32 {
    // SAFETY: zeroed storage is a valid out-buffer.
    let mut ss: libc::sockaddr_storage = unsafe { zeroed() };
    let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let c = loop {
        // SAFETY: valid out-pointers.
        #[cfg(unix)]
        let c = unsafe { libc::accept(fd, (&raw mut ss).cast(), &raw mut len) };
        // SAFETY: as above; no `fcntl` on preview 2 sockets.
        #[cfg(target_env = "p2")]
        let c =
            unsafe { libc::accept4(fd, (&raw mut ss).cast(), &raw mut len, libc::SOCK_NONBLOCK) };
        if c >= 0 || errno() != libc::EINTR {
            break c;
        }
    };
    if c < 0 {
        return -errno();
    }
    #[cfg(target_env = "p2")]
    let r = 0;
    #[cfg(unix)]
    let r = prepare(c);
    if r < 0 {
        // SAFETY: our fd.
        unsafe { libc::close(c) };
        return r;
    }
    write_addr(out, (&raw const ss).cast());
    c
}

#[cfg(any(unix, target_env = "p2"))]
/// The local (`peer == 0`) or peer address of `fd` into `out`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_addr(fd: i32, out: *mut u8, peer: i32) -> i32 {
    // SAFETY: zeroed storage is a valid out-buffer.
    let mut ss: libc::sockaddr_storage = unsafe { zeroed() };
    let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: valid out-pointers.
    let r = unsafe {
        if peer == 0 {
            libc::getsockname(fd, (&raw mut ss).cast(), &raw mut len)
        } else {
            libc::getpeername(fd, (&raw mut ss).cast(), &raw mut len)
        }
    };
    if r < 0 {
        return -errno();
    }
    write_addr(out, (&raw const ss).cast());
    0
}

/// The calls preview 1 has no socket API for.
#[cfg(all(target_os = "wasi", not(target_env = "p2")))]
mod p1 {
    use crate::str::RokStr;

    const UNSUPPORTED: i32 = -libc::ENOSYS;

    macro_rules! unsupported {
        ($($name:ident($($t:ty),*) -> $r:ty;)*) => {$(
            #[unsafe(no_mangle)]
            pub const extern "C" fn $name($(_: $t),*) -> $r {
                UNSUPPORTED as $r
            }
        )*};
    }

    unsupported! {
        rok_net_socket(i32, i32, i32) -> i32;
        rok_net_bind(i32, *mut u8) -> i32;
        rok_net_listen(i32, i32) -> i32;
        rok_net_connect(i32, *mut u8) -> i32;
        rok_net_sock_error(i32) -> i32;
        rok_net_addr(i32, *mut u8, i32) -> i32;
        rok_net_send_to(i32, *mut u8, usize, usize, *mut u8) -> isize;
        rok_net_recv_from(i32, *mut u8, usize, usize, *mut u8) -> isize;
        rok_net_setopt(i32, i32, i64) -> i32;
        rok_net_getopt(i32, i32, *mut u8) -> i32;
        rok_net_multicast_v4(i32, u32, u32, i32) -> i32;
        rok_net_resolve(RokStr, u16, *mut u8, usize) -> i32;
    }
}

/// Accept one connection (non-blocking: `-EAGAIN` if none is queued).
/// Preview 1 cannot tell the peer: `out` gets `0.0.0.0:0`.
#[cfg(all(target_os = "wasi", not(target_env = "p2")))]
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_accept(fd: i32, out: *mut u8) -> i32 {
    // Not in the `libc` crate for preview 1; wasi-libc has it.
    unsafe extern "C" {
        fn accept(fd: i32, addr: *mut u8, len: *mut u32) -> i32;
    }
    // SAFETY: the peer is not asked for.
    let c = unsafe { accept(fd, core::ptr::null_mut(), core::ptr::null_mut()) };
    if c < 0 {
        return -errno();
    }
    let r = prepare(c);
    if r < 0 {
        // SAFETY: our fd.
        unsafe { libc::close(c) };
        return r;
    }
    if !out.is_null() {
        for i in 0..ADDR_WORDS {
            // SAFETY: a live `ADDR_WORDS` buffer.
            unsafe { *word(out, i) = 0 };
        }
        // SAFETY: as above.
        unsafe { *word(out, 0) = 4 };
    }
    c
}

/// Listening sockets the host handed in, into the `GcBuf<i32>` `out`
/// (up to `max`): WASI's pre-opened sockets (wasmtime `-S tcplisten=`),
/// systemd's socket activation (`LISTEN_FDS` for this `LISTEN_PID`,
/// from fd 3) on Unix. Each is made non-blocking. The count.
#[unsafe(no_mangle)]
#[allow(clippy::cast_ptr_alignment)] // GC payloads are 8-aligned
pub extern "C" fn rok_net_listeners(out: *mut u8, max: usize) -> i32 {
    let mut n = 0;
    let mut fd = 3;
    while n < max {
        if !inherited(fd) {
            break;
        }
        if prepare(fd) == 0 {
            // SAFETY: the library passes a live `GcBuf<i32>` of `max`.
            unsafe { *out.add(HEADER_SIZE).cast::<i32>().add(n) = fd };
            n += 1;
        }
        fd += 1;
    }
    n as i32
}

/// WASI: pre-opens run from fd 3 without gaps; the sockets among them.
#[cfg(target_os = "wasi")]
fn inherited(fd: i32) -> bool {
    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    unsafe extern "C" {
        fn fd_fdstat_get(fd: i32, out: *mut [u64; 3]) -> u16;
    }
    const SOCKET_STREAM: u8 = 6;
    let mut st = [0u64; 3];
    // SAFETY: a 24-byte, 8-aligned `fdstat`.
    let e = unsafe { fd_fdstat_get(fd, &raw mut st) };
    e == 0 && st[0] as u8 == SOCKET_STREAM
}

/// Unix: systemd's `LISTEN_PID` / `LISTEN_FDS`.
#[cfg(unix)]
fn inherited(fd: i32) -> bool {
    let num = |name: &core::ffi::CStr| -> i64 {
        // SAFETY: getenv's result is null or NUL-terminated.
        let p = unsafe { libc::getenv(name.as_ptr()) };
        if p.is_null() {
            return -1;
        }
        // SAFETY: as above.
        let s = unsafe { core::ffi::CStr::from_ptr(p) }.to_bytes();
        core::str::from_utf8(s)
            .ok()
            .and_then(|t| t.parse().ok())
            .unwrap_or(-1)
    };
    // SAFETY: plain libc call.
    let me = i64::from(unsafe { libc::getpid() });
    num(c"LISTEN_PID") == me && i64::from(fd) < 3 + num(c"LISTEN_FDS")
}

/// Retry `f` (a libc `-1`-on-error call) across `EINTR`.
fn no_eintr(mut f: impl FnMut() -> isize) -> isize {
    loop {
        let n = ret(f());
        if n != -(libc::EINTR as isize) {
            return n;
        }
    }
}

/// One `send` of `buf[off..off + len]` (no SIGPIPE): the count or `-errno`.
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_net_send(fd: i32, buf: *mut u8, off: usize, len: usize) -> isize {
    // SAFETY: caller contract.
    let src = unsafe { region(buf, off) };
    // SAFETY: `src` is readable for `len` bytes.
    no_eintr(|| unsafe { libc::send(fd, src.cast(), len, SEND_FLAGS) })
}

/// One `recv` into `buf[off..off + len]`: the count (0 at the end) or
/// `-errno`. (WASI's preview 2 sockets are no files for `read`.)
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_net_recv(fd: i32, buf: *mut u8, off: usize, len: usize) -> isize {
    // SAFETY: caller contract.
    let dst = unsafe { region(buf, off) };
    let (n, _) = native(|| {
        // SAFETY: `dst` is writable for `len` bytes.
        no_eintr(|| unsafe { libc::recv(fd, dst.cast(), len, 0) })
    });
    n
}

#[cfg(any(unix, target_env = "p2"))]
/// One datagram `buf[off..off + len]` to `addr`: the count or `-errno`.
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_net_send_to(
    fd: i32,
    buf: *mut u8,
    off: usize,
    len: usize,
    addr: *mut u8,
) -> isize {
    let (ss, sl) = read_addr(addr);
    // SAFETY: caller contract.
    let src = unsafe { region(buf, off) };
    // SAFETY: valid buffer and sockaddr.
    no_eintr(|| unsafe {
        libc::sendto(fd, src.cast(), len, SEND_FLAGS, (&raw const ss).cast(), sl)
    })
}

#[cfg(any(unix, target_env = "p2"))]
/// One datagram into `buf[off..off + len]` (truncated to fit); the
/// sender goes to `out` (may be null). The count or `-errno`.
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_net_recv_from(
    fd: i32,
    buf: *mut u8,
    off: usize,
    len: usize,
    out: *mut u8,
) -> isize {
    // SAFETY: caller contract.
    let dst = unsafe { region(buf, off) };
    // SAFETY: zeroed storage is a valid out-buffer.
    let mut ss: libc::sockaddr_storage = unsafe { zeroed() };
    let mut sl = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: `dst` is writable for `len` bytes.
    let n = no_eintr(|| unsafe {
        libc::recvfrom(fd, dst.cast(), len, 0, (&raw mut ss).cast(), &raw mut sl)
    });
    if n >= 0 && sl > 0 {
        write_addr(out, (&raw const ss).cast());
    }
    n
}

/// `shutdown`: `how` 0 read, 1 write, 2 both.
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_shutdown(fd: i32, how: i32) -> i32 {
    // wasi-libc's values; preview 1's are not in the `libc` crate.
    #[cfg(all(target_os = "wasi", not(target_env = "p2")))]
    let h = match how {
        0 => 1,
        1 => 2,
        _ => 3,
    };
    #[cfg(any(unix, target_env = "p2"))]
    let h = match how {
        0 => libc::SHUT_RD,
        1 => libc::SHUT_WR,
        _ => libc::SHUT_RDWR,
    };
    // SAFETY: plain libc call.
    ret(unsafe { libc::shutdown(fd, h) } as isize) as i32
}

#[cfg(any(unix, target_env = "p2"))]
fn setopt(fd: i32, level: libc::c_int, name: libc::c_int, v: libc::c_int) -> i32 {
    // SAFETY: valid option pointer.
    let r = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            (&raw const v).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    ret(r as isize) as i32
}

#[cfg(any(unix, target_env = "p2"))]
fn getopt(fd: i32, level: libc::c_int, name: libc::c_int) -> Result<libc::c_int, i32> {
    let mut v: libc::c_int = 0;
    let mut n = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: valid out-pointers.
    let r = unsafe { libc::getsockopt(fd, level, name, (&raw mut v).cast(), &raw mut n) };
    if r < 0 { Err(-errno()) } else { Ok(v) }
}

/// `SO_LINGER` in seconds (plain `SO_LINGER` counts ticks on Apple).
#[cfg(target_vendor = "apple")]
const LINGER: libc::c_int = libc::SO_LINGER_SEC;
#[cfg(all(unix, not(target_vendor = "apple")))]
const LINGER: libc::c_int = libc::SO_LINGER;

#[cfg(unix)]
fn set_linger(fd: i32, secs: libc::c_int) -> i32 {
    let l = libc::linger {
        l_onoff: libc::c_int::from(secs >= 0),
        l_linger: secs.max(0),
    };
    // SAFETY: a valid option.
    let r = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            LINGER,
            (&raw const l).cast(),
            size_of::<libc::linger>() as libc::socklen_t,
        )
    };
    ret(r as isize) as i32
}

#[cfg(any(unix, target_env = "p2"))]
/// Socket options by library code: 1 `TCP_NODELAY`, 2 keepalive (`val`
/// idle seconds, < 0 off), 3 `SO_BROADCAST`, 4 `IP_MULTICAST_LOOP`, 5
/// `IP_MULTICAST_TTL`, 6 `UDP_SEGMENT` (Linux only), 7 `SO_REUSEADDR`, 8
/// `SO_REUSEPORT`, 9 `SO_RCVBUF`, 10 `SO_SNDBUF`, 11 `IP_TTL`, 12
/// `IPV6_V6ONLY`, 13 `SO_LINGER` (`val` seconds, < 0 off).
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_setopt(fd: i32, opt: i32, val: i64) -> i32 {
    let v = val as libc::c_int;
    match opt {
        1 => setopt(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, v),
        2 => {
            let r = setopt(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE, i32::from(v >= 0));
            if r < 0 || v < 0 {
                return r;
            }
            #[cfg(target_vendor = "apple")]
            let idle = libc::TCP_KEEPALIVE;
            #[cfg(not(target_vendor = "apple"))]
            let idle = libc::TCP_KEEPIDLE;
            setopt(fd, libc::IPPROTO_TCP, idle, v.max(1))
        }
        3 => setopt(fd, libc::SOL_SOCKET, libc::SO_BROADCAST, v),
        4 => {
            // SAFETY: a one-byte option on BSD.
            #[cfg(target_vendor = "apple")]
            unsafe {
                let b = u8::from(v != 0);
                ret(libc::setsockopt(
                    fd,
                    libc::IPPROTO_IP,
                    libc::IP_MULTICAST_LOOP,
                    (&raw const b).cast(),
                    1,
                ) as isize) as i32
            }
            #[cfg(not(target_vendor = "apple"))]
            setopt(fd, libc::IPPROTO_IP, libc::IP_MULTICAST_LOOP, v)
        }
        5 => setopt(fd, libc::IPPROTO_IP, libc::IP_MULTICAST_TTL, v),
        #[cfg(target_os = "linux")]
        6 => setopt(fd, libc::SOL_UDP, libc::UDP_SEGMENT, v),
        7 => setopt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, v),
        #[cfg(unix)]
        8 => setopt(fd, libc::SOL_SOCKET, libc::SO_REUSEPORT, v),
        #[cfg(unix)]
        9 => setopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF, v),
        #[cfg(unix)]
        10 => setopt(fd, libc::SOL_SOCKET, libc::SO_SNDBUF, v),
        #[cfg(unix)]
        11 => setopt(fd, libc::IPPROTO_IP, libc::IP_TTL, v),
        #[cfg(unix)]
        12 => setopt(fd, libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, v),
        #[cfg(unix)]
        13 => set_linger(fd, v),
        _ => -libc::ENOTSUP,
    }
}

#[cfg(any(unix, target_env = "p2"))]
/// The value of option `opt` (as in [`rok_net_setopt`]) into word 0 of
/// the `GcBuf<i64>` `out`: 0 / 1 for a flag, a size, a TTL, or the linger
/// seconds (-1: off). 0 or `-errno`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_getopt(fd: i32, opt: i32, out: *mut u8) -> i32 {
    let v = match opt {
        1 => getopt(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY),
        3 => getopt(fd, libc::SOL_SOCKET, libc::SO_BROADCAST),
        7 => getopt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR),
        #[cfg(unix)]
        8 => getopt(fd, libc::SOL_SOCKET, libc::SO_REUSEPORT),
        #[cfg(unix)]
        9 => getopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF),
        #[cfg(unix)]
        10 => getopt(fd, libc::SOL_SOCKET, libc::SO_SNDBUF),
        #[cfg(unix)]
        11 => getopt(fd, libc::IPPROTO_IP, libc::IP_TTL),
        #[cfg(unix)]
        12 => getopt(fd, libc::IPPROTO_IPV6, libc::IPV6_V6ONLY),
        #[cfg(unix)]
        13 => {
            // SAFETY: zeroed is a valid linger.
            let mut l: libc::linger = unsafe { zeroed() };
            let mut n = size_of::<libc::linger>() as libc::socklen_t;
            // SAFETY: valid out-pointers.
            let r = unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    LINGER,
                    (&raw mut l).cast(),
                    &raw mut n,
                )
            };
            if r < 0 {
                Err(-errno())
            } else if l.l_onoff == 0 {
                Ok(-1)
            } else {
                Ok(l.l_linger)
            }
        }
        _ => Err(-libc::ENOTSUP),
    };
    match v {
        Ok(v) => {
            // SAFETY: the library passes a live `GcBuf<i64>` of one word.
            unsafe { *word(out, 0) = i64::from(v) };
            0
        }
        Err(e) => e,
    }
}

#[cfg(any(unix, target_env = "p2"))]
/// Join (`join != 0`) or leave IPv4 multicast `group` on `iface` (both
/// as `a.b.c.d` octets in a big-endian `u32`).
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_multicast_v4(fd: i32, group: u32, iface: u32, join: i32) -> i32 {
    let m = libc::ip_mreq {
        imr_multiaddr: libc::in_addr {
            s_addr: group.to_be(),
        },
        imr_interface: libc::in_addr {
            s_addr: iface.to_be(),
        },
    };
    let name = if join != 0 {
        libc::IP_ADD_MEMBERSHIP
    } else {
        libc::IP_DROP_MEMBERSHIP
    };
    // SAFETY: valid option pointer.
    let r = unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_IP,
            name,
            (&raw const m).cast(),
            size_of::<libc::ip_mreq>() as libc::socklen_t,
        )
    };
    ret(r as isize) as i32
}

/// Block (parked for the GC) until `fd` is readable (`write == 0`) or
/// writable, at most `timeout_ns` (< 0: no limit). 1 ready, 0 timed out,
/// or `-errno`. Hang-ups and errors count as ready (the next call
/// reports them).
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_wait(fd: i32, write: i32, timeout_ns: i64) -> i32 {
    let mut p = libc::pollfd {
        fd,
        events: if write != 0 {
            libc::POLLOUT
        } else {
            libc::POLLIN
        },
        revents: 0,
    };
    let ms = if timeout_ns < 0 {
        -1
    } else {
        // Round up: a 1 ns timeout still waits one tick.
        (timeout_ns.saturating_add(999_999) / 1_000_000).min(i64::from(i32::MAX)) as libc::c_int
    };
    let mut poll = |ms| {
        loop {
            // SAFETY: one valid pollfd.
            let r = unsafe { libc::poll(&raw mut p, 1, ms) };
            if r >= 0 || errno() != libc::EINTR {
                break r;
            }
        }
    };
    if ms == 0 {
        let r = poll(0);
        return if r < 0 { -errno() } else { i32::from(r > 0) };
    }
    let (r, e) = native(|| poll(ms));
    if r < 0 { -e } else { i32::from(r > 0) }
}

#[cfg(any(unix, target_env = "p2"))]
/// Resolve `host` (a name or numeric address) with `port`: up to `max`
/// addresses into `out` (`max * ADDR_WORDS` words), stream sockets only.
/// The count or `-code` (an errno, or a resolver code from `EAI_BASE`).
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_resolve(host: RokStr, port: u16, out: *mut u8, max: usize) -> i32 {
    // SAFETY: zeroed hints are valid; `cs` NUL-terminates `host`.
    let mut hints: libc::addrinfo = unsafe { zeroed() };
    hints.ai_socktype = libc::SOCK_STREAM;
    let mut res: *mut libc::addrinfo = core::ptr::null_mut();
    let (r, e) = native(|| unsafe {
        libc::getaddrinfo(
            cs(host).p(),
            core::ptr::null(),
            &raw const hints,
            &raw mut res,
        )
    });
    if r != 0 {
        if r == libc::EAI_SYSTEM {
            return -e;
        }
        return -(EAI_BASE + r.abs());
    }
    let mut n = 0;
    let mut p = res;
    while !p.is_null() && n < max {
        // SAFETY: a list from getaddrinfo; `out` has room for `max`.
        unsafe {
            let fam = (*p).ai_family;
            if fam == libc::AF_INET || fam == libc::AF_INET6 {
                let slot = out.add(n * ADDR_WORDS * 8);
                write_addr(slot, (*p).ai_addr);
                *word(slot, 1) = i64::from(port);
                n += 1;
            }
            p = (*p).ai_next;
        }
    }
    // SAFETY: from getaddrinfo.
    unsafe { libc::freeaddrinfo(res) };
    n as i32
}

// Unix domain sockets

/// `path` as a `sockaddr_un` and its length; `None` when it does not fit
/// `sun_path` (with its NUL).
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // `c_char` signedness varies by target
fn unix_addr(path: &[u8]) -> Option<(libc::sockaddr_un, libc::socklen_t)> {
    // SAFETY: zeroed is a valid `sockaddr_un`.
    let mut sa: libc::sockaddr_un = unsafe { zeroed() };
    if path.len() >= sa.sun_path.len() {
        return None;
    }
    sa.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (d, s) in sa.sun_path.iter_mut().zip(path) {
        *d = *s as libc::c_char;
    }
    let len = core::mem::offset_of!(libc::sockaddr_un, sun_path) + path.len() + 1;
    #[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
    {
        sa.sun_len = len as u8;
    }
    Some((sa, len as libc::socklen_t))
}

/// A Unix socket (`kind` 1 stream / 2 datagram) at `path`: `op` 0 leaves
/// it unbound, 1 binds it (and listens, backlog 128, for a stream), 2
/// connects it (blocking, before it turns non-blocking). The fd or
/// `-errno` (`-ENAMETOOLONG` for a path `sun_path` cannot hold).
#[cfg(unix)]
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_unix(kind: i32, path: RokStr, op: i32) -> i32 {
    let ty = if kind == 2 {
        libc::SOCK_DGRAM
    } else {
        libc::SOCK_STREAM
    };
    let Some((sa, len)) = unix_addr(path.bytes()) else {
        return -libc::ENAMETOOLONG;
    };
    // SAFETY: plain libc calls on our own fd and a valid sockaddr.
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, ty, 0);
        if fd < 0 {
            return -errno();
        }
        let addr = (&raw const sa).cast::<libc::sockaddr>();
        let r = match op {
            1 => {
                let r = libc::bind(fd, addr, len);
                if r == 0 && ty == libc::SOCK_STREAM {
                    libc::listen(fd, 128)
                } else {
                    r
                }
            }
            2 => loop {
                let (r, e) = native(|| libc::connect(fd, addr, len));
                if r == 0 || e != libc::EINTR {
                    break r;
                }
            },
            _ => 0,
        };
        let r = if r < 0 { -errno() } else { prepare(fd) };
        if r < 0 {
            libc::close(fd);
            return r;
        }
        fd
    }
}

/// `socketpair(AF_UNIX, kind)`, both ends non-blocking + close-on-exec,
/// into the `GcBuf<i32>` `out`: 0 or `-errno`.
#[cfg(unix)]
#[unsafe(no_mangle)]
#[allow(clippy::cast_ptr_alignment)] // GC payloads are 8-aligned
pub extern "C" fn rok_net_unix_pair(kind: i32, out: *mut u8) -> i32 {
    let ty = if kind == 2 {
        libc::SOCK_DGRAM
    } else {
        libc::SOCK_STREAM
    };
    let mut fds = [0i32; 2];
    // SAFETY: `fds` holds two fds; `out` is a live two-element buffer.
    unsafe {
        if libc::socketpair(libc::AF_UNIX, ty, 0, fds.as_mut_ptr()) < 0 {
            return -errno();
        }
        for fd in fds {
            let r = prepare(fd);
            if r < 0 {
                libc::close(fds[0]);
                libc::close(fds[1]);
                return r;
            }
        }
        let o = out.add(HEADER_SIZE).cast::<i32>();
        *o = fds[0];
        *o.add(1) = fds[1];
    }
    0
}

/// The local (`peer == 0`) or peer path of Unix socket `fd` into
/// `buf[off..off + len]`: its length (0: unnamed) or `-errno`.
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + len` bytes.
#[cfg(unix)]
#[unsafe(no_mangle)]
#[allow(clippy::unnecessary_cast)] // `c_char` signedness varies by target
pub unsafe extern "C" fn rok_net_unix_path(
    fd: i32,
    peer: i32,
    buf: *mut u8,
    off: usize,
    len: usize,
) -> isize {
    // SAFETY: zeroed storage is a valid out-buffer; caller contract on
    // `buf`.
    unsafe {
        let mut sa: libc::sockaddr_un = zeroed();
        let mut n = size_of::<libc::sockaddr_un>() as libc::socklen_t;
        let p = (&raw mut sa).cast::<libc::sockaddr>();
        let r = if peer != 0 {
            libc::getpeername(fd, p, &raw mut n)
        } else {
            libc::getsockname(fd, p, &raw mut n)
        };
        if r < 0 {
            let e = errno();
            // A peer that already hung up has no name to report (macOS
            // says EINVAL, Linux ENOTCONN): unnamed.
            if peer != 0 && (e == libc::EINVAL || e == libc::ENOTCONN) {
                return 0;
            }
            return -(e as isize);
        }
        let start = core::mem::offset_of!(libc::sockaddr_un, sun_path);
        let room = (n as usize).saturating_sub(start).min(sa.sun_path.len());
        let path = &sa.sun_path[..room];
        let k = path.iter().position(|c| *c == 0).unwrap_or(room).min(len);
        let dst = region(buf, off);
        for (i, c) in path[..k].iter().enumerate() {
            *dst.add(i) = *c as u8;
        }
        k as isize
    }
}

/// Room for one fd in a control message.
#[cfg(unix)]
#[repr(C)]
union FdCmsg {
    buf: [u8; 64],
    _align: libc::cmsghdr,
}

/// `buf[off..off + len]` with fd `pass` as `SCM_RIGHTS`: the count or
/// `-errno`.
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + len` bytes.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_net_send_fd(
    fd: i32,
    buf: *mut u8,
    off: usize,
    len: usize,
    pass: i32,
) -> isize {
    // SAFETY: the control buffer has room for one fd's cmsg; caller
    // contract on `buf`.
    unsafe {
        let mut iov = libc::iovec {
            iov_base: region(buf, off).cast(),
            iov_len: len,
        };
        let mut ctl = FdCmsg { buf: [0; 64] };
        let mut msg: libc::msghdr = zeroed();
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = (&raw mut ctl).cast();
        msg.msg_controllen = libc::CMSG_SPACE(size_of::<i32>() as u32) as _;
        let c = libc::CMSG_FIRSTHDR(&raw const msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(size_of::<i32>() as u32) as _;
        libc::CMSG_DATA(c).cast::<i32>().write_unaligned(pass);
        no_eintr(|| libc::sendmsg(fd, &raw const msg, SEND_FLAGS))
    }
}

/// One receive into `buf[off..off + len]`; a passed fd (made
/// close-on-exec) goes to `out[0]`, else -1. The count or `-errno`.
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + len` bytes and
/// `out` a live one-element `GcBuf<i32>`.
#[cfg(unix)]
#[unsafe(no_mangle)]
#[allow(clippy::cast_ptr_alignment)] // GC payloads are 8-aligned
pub unsafe extern "C" fn rok_net_recv_fd(
    fd: i32,
    buf: *mut u8,
    off: usize,
    len: usize,
    out: *mut u8,
) -> isize {
    // SAFETY: as for `rok_net_send_fd`; caller contract on `out`.
    unsafe {
        let got = out.add(HEADER_SIZE).cast::<i32>();
        *got = -1;
        let mut iov = libc::iovec {
            iov_base: region(buf, off).cast(),
            iov_len: len,
        };
        let mut ctl = FdCmsg { buf: [0; 64] };
        let mut msg: libc::msghdr = zeroed();
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = (&raw mut ctl).cast();
        msg.msg_controllen = size_of::<FdCmsg>() as _;
        #[cfg(target_os = "linux")]
        let flags = libc::MSG_CMSG_CLOEXEC;
        #[cfg(not(target_os = "linux"))]
        let flags = 0;
        let n = no_eintr(|| libc::recvmsg(fd, &raw mut msg, flags));
        if n < 0 {
            return n;
        }
        let mut c = libc::CMSG_FIRSTHDR(&raw const msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let f = libc::CMSG_DATA(c).cast::<i32>().read_unaligned();
                #[cfg(not(target_os = "linux"))]
                libc::fcntl(f, libc::F_SETFD, libc::FD_CLOEXEC);
                *got = f;
                break;
            }
            c = libc::CMSG_NXTHDR(&raw const msg, c);
        }
        n
    }
}

/// One `sendfile(2)` of `file[offset..offset + count]` to socket `sock`
/// (Linux): the count (0 at the file's end) or `-errno`; `-ENOSYS`
/// elsewhere.
#[unsafe(no_mangle)]
#[cfg_attr(not(target_os = "linux"), allow(clippy::missing_const_for_fn))]
pub extern "C" fn rok_net_sendfile(sock: i32, file: i32, offset: i64, count: usize) -> isize {
    #[cfg(target_os = "linux")]
    {
        let mut off = offset as libc::off_t;
        // SAFETY: plain syscall with a valid offset pointer.
        no_eintr(|| unsafe { libc::sendfile(sock, file, &raw mut off, count) })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (sock, file, offset, count);
        -(libc::ENOSYS as isize)
    }
}

/// One `send` of `buf[off..off + len]` with `MSG_ZEROCOPY` (Linux 4.14+):
/// the count or `-errno`; `-ENOSYS` elsewhere. The kernel reads the bytes
/// in place, so the buffer stays pinned until the socket's error queue
/// reports the send complete (it is a list's own storage, which nothing
/// else pins).
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + len` bytes.
#[unsafe(no_mangle)]
#[cfg_attr(not(target_os = "linux"), allow(clippy::missing_const_for_fn))]
pub unsafe extern "C" fn rok_net_send_zerocopy(
    fd: i32,
    buf: *mut u8,
    off: usize,
    len: usize,
) -> isize {
    #[cfg(target_os = "linux")]
    {
        const SO_EE_ORIGIN_ZEROCOPY: u8 = 5;
        let one: libc::c_int = 1;
        // SAFETY: plain syscall on the caller's socket.
        let r = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ZEROCOPY,
                (&raw const one).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if r < 0 {
            return -(errno() as isize);
        }
        // SAFETY: caller contract.
        let src = unsafe { region(buf, off) };
        // SAFETY: a live GC object, unpinned below.
        unsafe { rokr_gc::pin(buf) };
        // SAFETY: `src` is readable for `len` bytes.
        let n = no_eintr(|| unsafe {
            libc::send(fd, src.cast(), len, SEND_FLAGS | libc::MSG_ZEROCOPY)
        });
        if n > 0 {
            // Wait for the completion notice (parked: the buffer is pinned).
            native(|| {
                loop {
                    let mut ctrl = [0u64; 16];
                    // SAFETY: a zeroed header pointing at our control buffer.
                    let mut msg: libc::msghdr = unsafe { core::mem::zeroed() };
                    msg.msg_control = ctrl.as_mut_ptr().cast();
                    msg.msg_controllen = size_of_val(&ctrl) as _;
                    // SAFETY: plain syscall with a valid header.
                    let got = unsafe { libc::recvmsg(fd, &raw mut msg, libc::MSG_ERRQUEUE) };
                    if got >= 0 {
                        // SAFETY: walking the control messages the kernel wrote.
                        let done = unsafe {
                            let mut cm = libc::CMSG_FIRSTHDR(&raw const msg);
                            let mut done = false;
                            while !cm.is_null() {
                                let ext = libc::CMSG_DATA(cm)
                                    .cast::<libc::sock_extended_err>()
                                    .read_unaligned();
                                done |= ext.ee_origin == SO_EE_ORIGIN_ZEROCOPY;
                                cm = libc::CMSG_NXTHDR(&raw const msg, cm);
                            }
                            done
                        };
                        if done {
                            break;
                        }
                        continue;
                    }
                    match errno() {
                        libc::EINTR => {}
                        libc::EAGAIN => {
                            let mut pfd = libc::pollfd {
                                fd,
                                events: 0,
                                revents: 0,
                            };
                            // SAFETY: plain syscall; POLLERR is always reported.
                            unsafe { libc::poll(&raw mut pfd, 1, -1) };
                        }
                        _ => break,
                    }
                }
            });
        }
        // SAFETY: pinned above.
        unsafe { rokr_gc::unpin(buf) };
        n
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (fd, buf, off, len);
        -(libc::ENOSYS as isize)
    }
}

/// Up to `count` bytes from `file`'s offset to socket `sock` through a
/// pipe with `splice(2)` (Linux): the count (0 at the file's end) or
/// `-errno`; `-ENOSYS` elsewhere. What reaches the pipe is drained to
/// the socket before returning, waiting for it to take more.
#[unsafe(no_mangle)]
#[cfg_attr(not(target_os = "linux"), allow(clippy::missing_const_for_fn))]
pub extern "C" fn rok_net_splice(sock: i32, file: i32, count: usize) -> isize {
    #[cfg(target_os = "linux")]
    {
        let mut p = [0i32; 2];
        // SAFETY: `p` holds two fds.
        if unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
            return -(errno() as isize);
        }
        let (n, e) = native(|| {
            // SAFETY: plain syscalls on fds the caller and we own.
            unsafe {
                let null = ptr::null_mut::<libc::loff_t>();
                let n = loop {
                    let n = libc::splice(file, null, p[1], null, count, libc::SPLICE_F_MOVE);
                    if n >= 0 || errno() != libc::EINTR {
                        break n;
                    }
                };
                let mut left = n.max(0) as usize;
                while left > 0 {
                    let k = libc::splice(p[0], null, sock, null, left, libc::SPLICE_F_MOVE);
                    if k > 0 {
                        left -= k as usize;
                        continue;
                    }
                    match errno() {
                        libc::EINTR => {}
                        libc::EAGAIN => {
                            let mut pfd = libc::pollfd {
                                fd: sock,
                                events: libc::POLLOUT,
                                revents: 0,
                            };
                            libc::poll(&raw mut pfd, 1, -1);
                        }
                        _ => return -1,
                    }
                }
                n
            }
        });
        // SAFETY: our pipe.
        unsafe {
            libc::close(p[0]);
            libc::close(p[1]);
        }
        if n < 0 { -(e as isize) } else { n }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (sock, file, count);
        -(libc::ENOSYS as isize)
    }
}

// The I/O driver's poller

#[cfg(not(target_os = "wasi"))]
static POLLER: AtomicI32 = AtomicI32::new(-1);

/// The process's kqueue / epoll fd, created on first use.
#[cfg(not(target_os = "wasi"))]
#[unsafe(no_mangle)]
pub extern "C" fn rok_io_poller() -> i32 {
    let p = POLLER.load(Ordering::Acquire);
    if p >= 0 {
        return p;
    }
    // SAFETY: plain libc calls.
    #[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
    let fd = unsafe {
        let fd = libc::kqueue();
        if fd >= 0 {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        fd
    };
    // SAFETY: plain libc call.
    #[cfg(target_os = "linux")]
    let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if fd < 0 {
        rokr_common::fatal(b"rok: cannot create the I/O poller");
    }
    match POLLER.compare_exchange(-1, fd, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => fd,
        Err(cur) => {
            // SAFETY: our own extra fd.
            unsafe { libc::close(fd) };
            cur
        }
    }
}

/// After a failed REPL cell (`session`): a new poller on next use (a
/// forked copy has no kqueue; an epoll set would be shared with the
/// process that died). Waiters watch their fds again when woken.
#[cfg(not(target_os = "wasi"))]
pub fn reset_poller() {
    let p = POLLER.swap(-1, Ordering::AcqRel);
    if p >= 0 {
        // SAFETY: our own fd, no longer published.
        unsafe { libc::close(p) };
    }
}

#[cfg(not(target_os = "wasi"))]
/// Watch `fd` for readability and writability, edge-triggered (an
/// event per change, so a waiter must re-check after it parks). Watching
/// a watched fd again is harmless. `interest` (1 read, 2 write): what the
/// caller waits for; kqueue / epoll watch both. 0 or `-errno`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_io_watch(fd: i32, _interest: u32) -> i32 {
    let p = rok_io_poller();
    #[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
    {
        // SAFETY: zeroed kevents are valid; filled below.
        let mut ch: [libc::kevent; 2] = unsafe { zeroed() };
        for (k, filter) in ch.iter_mut().zip([libc::EVFILT_READ, libc::EVFILT_WRITE]) {
            k.ident = fd as libc::uintptr_t;
            k.filter = filter;
            k.flags = libc::EV_ADD | libc::EV_CLEAR;
        }
        // SAFETY: two valid changes, no event buffer.
        let r = unsafe {
            libc::kevent(
                p,
                ch.as_ptr(),
                2,
                core::ptr::null_mut(),
                0,
                core::ptr::null(),
            )
        };
        ret(r as isize) as i32
    }
    #[cfg(target_os = "linux")]
    {
        let mut ev = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLOUT | libc::EPOLLRDHUP | libc::EPOLLET) as u32,
            u64: fd as u64,
        };
        // SAFETY: a valid event.
        let r = unsafe { libc::epoll_ctl(p, libc::EPOLL_CTL_ADD, fd, &raw mut ev) };
        if r < 0 && errno() == libc::EEXIST {
            return 0;
        }
        ret(r as isize) as i32
    }
}

#[cfg(not(target_os = "wasi"))]
/// Block (parked for the GC) until watched fds change; up to `max`
/// events go to the `GcBuf<i64>` `out` as `fd * 2 + (0 read | 1 write)`.
/// The count, or `-errno`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_io_poll(out: *mut u8, max: usize) -> i32 {
    const N: usize = 64;
    let p = rok_io_poller();
    let mut keys = [0i64; 2 * N];
    let (n, e) = native(|| {
        let mut n = 0usize;
        #[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
        {
            // SAFETY: zeroed kevents are a valid out-buffer.
            let mut evs: [libc::kevent; N] = unsafe { zeroed() };
            // SAFETY: valid buffer; no timeout.
            let r = unsafe {
                libc::kevent(
                    p,
                    core::ptr::null(),
                    0,
                    evs.as_mut_ptr(),
                    N as libc::c_int,
                    core::ptr::null(),
                )
            };
            if r < 0 {
                return -1;
            }
            for ev in &evs[..r as usize] {
                let dir = i64::from(ev.filter == libc::EVFILT_WRITE);
                keys[n] = ev.ident as i64 * 2 + dir;
                n += 1;
            }
        }
        #[cfg(target_os = "linux")]
        {
            // SAFETY: zeroed events are a valid out-buffer.
            let mut evs: [libc::epoll_event; N] = unsafe { zeroed() };
            // SAFETY: valid buffer.
            let r = unsafe { libc::epoll_wait(p, evs.as_mut_ptr(), N as libc::c_int, -1) };
            if r < 0 {
                return -1;
            }
            for ev in &evs[..r as usize] {
                let fd = ev.u64 as i64;
                let bits = ev.events;
                let err = (libc::EPOLLERR | libc::EPOLLHUP) as u32;
                if bits & (libc::EPOLLIN as u32 | libc::EPOLLRDHUP as u32 | err) != 0 {
                    keys[n] = fd * 2;
                    n += 1;
                }
                if bits & (libc::EPOLLOUT as u32 | err) != 0 {
                    keys[n] = fd * 2 + 1;
                    n += 1;
                }
            }
        }
        n as isize
    });
    if n < 0 {
        return if e == libc::EINTR { 0 } else { -e };
    }
    let n = (n as usize).min(max);
    for (i, k) in keys[..n].iter().enumerate() {
        // SAFETY: the library passes `max` words.
        unsafe { *word(out, i) = *k };
    }
    n as i32
}

/// WASI: no kqueue / epoll. The watched fds and the directions still
/// armed (bit 0 read, 1 write); `poll` (`poll_oneoff`) over them reports
/// each armed direction once, like `EPOLLET`, until `rok_io_watch`
/// re-arms it.
#[cfg(target_os = "wasi")]
mod watch {
    use core::cell::UnsafeCell;

    use rokr_common::sys::Lock;

    /// Highest watchable fd + 1.
    pub const MAX: usize = 1024;

    pub struct Set {
        pub lock: Lock,
        pub armed: UnsafeCell<[u8; MAX]>,
    }

    // SAFETY: `armed` only under `lock`.
    unsafe impl Sync for Set {}

    pub static SET: Set = Set {
        lock: Lock::new(),
        armed: UnsafeCell::new([0; MAX]),
    };
}

/// No poller fd on WASI: the watch set is the runtime's.
#[cfg(target_os = "wasi")]
#[unsafe(no_mangle)]
pub const extern "C" fn rok_io_poller() -> i32 {
    0
}

/// Arm `fd` for the directions of `interest` (1 read, 2 write; only
/// those: wasi-libc rejects a `poll` for writing on a listening socket).
/// 0 or `-errno` (`-EMFILE` past the set).
#[cfg(target_os = "wasi")]
#[unsafe(no_mangle)]
pub extern "C" fn rok_io_watch(fd: i32, interest: u32) -> i32 {
    let Ok(i) = usize::try_from(fd) else {
        return -libc::EBADF;
    };
    if i >= watch::MAX {
        return -libc::EMFILE;
    }
    watch::SET.lock.lock();
    // SAFETY: under the lock.
    unsafe { (*watch::SET.armed.get())[i] |= interest as u8 & 3 };
    watch::SET.lock.unlock();
    0
}

/// One `poll` of the armed fds, at most `ms` (< 0: no limit): their
/// events (disarmed) into `out` as in [`rok_io_poll`]. `None`: nothing
/// is armed.
#[cfg(target_os = "wasi")]
fn poll_armed(out: *mut u8, max: usize, ms: i32) -> Option<i32> {
    const N: usize = 64;
    let mut fds = [libc::pollfd {
        fd: -1,
        events: 0,
        revents: 0,
    }; N];
    let mut k = 0;
    watch::SET.lock.lock();
    // SAFETY: under the lock.
    let armed = unsafe { &*watch::SET.armed.get() };
    for (fd, a) in armed.iter().enumerate() {
        if *a != 0 && k < N {
            let mut ev = 0;
            if a & 1 != 0 {
                ev |= libc::POLLIN;
            }
            if a & 2 != 0 {
                ev |= libc::POLLOUT;
            }
            fds[k] = libc::pollfd {
                fd: fd as i32,
                events: ev,
                revents: 0,
            };
            k += 1;
        }
    }
    watch::SET.lock.unlock();
    if k == 0 {
        return None;
    }
    // SAFETY: `k` valid pollfds.
    let (r, e) = native(|| unsafe { libc::poll(fds.as_mut_ptr(), k as libc::nfds_t, ms) });
    if r < 0 && e == libc::EINTR {
        return Some(0);
    }
    let mut n = 0;
    watch::SET.lock.lock();
    for p in &fds[..k] {
        // A failed `poll` wakes every waiter: its retried call reports
        // what is wrong with its socket.
        let hup = r < 0 || p.revents & (libc::POLLERR | libc::POLLHUP) != 0;
        for (dir, bit) in [(0, libc::POLLIN), (1, libc::POLLOUT)] {
            if n < max && p.events & bit != 0 && (p.revents & bit != 0 || hup) {
                // SAFETY: under the lock; the library passes `max` words.
                unsafe {
                    (*watch::SET.armed.get())[p.fd as usize] &= !(1 << dir);
                    *word(out, n) = i64::from(p.fd) * 2 + dir;
                }
                n += 1;
            }
        }
    }
    watch::SET.lock.unlock();
    Some(n as i32)
}

/// [`rok_io_poll`] for the driver thread (`wasm32-wasip1-threads`): a
/// watch armed meanwhile is seen within a 10 ms slice.
#[cfg(target_os = "wasi")]
#[unsafe(no_mangle)]
pub extern "C" fn rok_io_poll(out: *mut u8, max: usize) -> i32 {
    loop {
        match poll_armed(out, max, 10) {
            Some(0) => {}
            Some(n) => return n,
            None => rokr_common::sys::sleep_ns(10_000_000),
        }
    }
}

/// Whether a watch is armed: what the one-thread executor could wait for.
#[cfg(target_os = "wasi")]
#[unsafe(no_mangle)]
pub extern "C" fn rok_io_armed() -> i32 {
    watch::SET.lock.lock();
    // SAFETY: under the lock.
    let any = unsafe { &*watch::SET.armed.get() }.iter().any(|a| *a != 0);
    watch::SET.lock.unlock();
    i32::from(any)
}

/// The one-thread executor's wait: the watched fds' events (as
/// [`rok_io_poll`]) or `timeout_ns` (< 0: no limit), whichever comes
/// first; the count (0: timed out). Nothing watched and no limit is a
/// deadlock: nothing could ever wake the thread.
#[cfg(target_os = "wasi")]
#[unsafe(no_mangle)]
pub extern "C" fn rok_io_wait(out: *mut u8, max: usize, timeout_ns: i64) -> i32 {
    let ms = if timeout_ns < 0 {
        -1
    } else {
        (timeout_ns.saturating_add(999_999) / 1_000_000).min(i64::from(i32::MAX)) as i32
    };
    match poll_armed(out, max, ms) {
        Some(n) => n,
        None if timeout_ns < 0 => {
            rokr_common::fatal(b"fatal error: deadlock: the only thread waits for another")
        }
        None => {
            rokr_common::sys::sleep_ns(timeout_ns as u64);
            0
        }
    }
}
