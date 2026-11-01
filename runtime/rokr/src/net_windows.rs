//! Winsock sockets behind `rok::std::net` (stdlib 302) and the I/O
//! driver's poller (707) on Windows. Same `rok_net_*` / `rok_io_*`
//! symbols and conventions as the POSIX `net`; error codes are Winsock
//! codes. A socket's fd is its handle with bit 0 set (`os::fd_of`).
//!
//! The driver is readiness on a completion port (707 §7.7.1): each
//! watched socket has at most one `IOCTL_AFD_POLL` request in flight on
//! a shared `\Device\Afd` handle bound to the port. The request
//! completes (on the port) once any of the socket's armed directions is
//! ready. `rok_io_poll` reports those directions and disarms them, then
//! re-arms the rest; `rok_io_watch` re-arms both. That gives the epoll
//! `EPOLLET` contract the library relies on: one event per readiness
//! after each watch, spurious events allowed.

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::mem::{size_of, zeroed};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use rokr_common::object_header::HEADER_SIZE;
use rokr_common::sys;
use rokr_common::win::{self, HANDLE, SOCKET};

use crate::os::{fd_of, native, region, sock_of};
use crate::str::RokStr;

const ADDR_WORDS: usize = 20;

const AF_INET: i32 = 2;
const AF_INET6: i32 = 23;
const SOCK_STREAM: i32 = 1;
const SOCK_DGRAM: i32 = 2;
const SOCK_RAW: i32 = 3;
const SOL_SOCKET: i32 = 0xFFFF;
const SO_REUSEADDR: i32 = 0x4;
const SO_KEEPALIVE: i32 = 0x8;
const SO_BROADCAST: i32 = 0x20;
const SO_LINGER: i32 = 0x80;
const SO_SNDBUF: i32 = 0x1001;
const SO_RCVBUF: i32 = 0x1002;
const SO_ERROR: i32 = 0x1007;
const IPPROTO_IP: i32 = 0;
const IPPROTO_TCP: i32 = 6;
const IPPROTO_IPV6: i32 = 41;
const IP_TTL: i32 = 4;
const IPV6_V6ONLY: i32 = 27;
const TCP_NODELAY: i32 = 1;
const TCP_KEEPIDLE: i32 = 3;
const IP_MULTICAST_TTL: i32 = 10;
const IP_MULTICAST_LOOP: i32 = 11;
const IP_ADD_MEMBERSHIP: i32 = 12;
const IP_DROP_MEMBERSHIP: i32 = 13;
const FIONBIO: i32 = 0x8004_667E_u32 as i32;
const SIO_BASE_HANDLE: u32 = 0x4800_0022;
const WSA_FLAG_OVERLAPPED: u32 = 0x1;
const WSA_FLAG_NO_HANDLE_INHERIT: u32 = 0x80;
const POLLERR: i16 = 0x1;
const POLLHUP: i16 = 0x2;
const POLLWRNORM: i16 = 0x10;
const POLLRDNORM: i16 = 0x100;
const SOCKET_ERROR: i32 = -1;
const WSAEMSGSIZE: i32 = 10040;

#[repr(C)]
struct SockaddrIn {
    family: u16,
    port: u16,
    addr: [u8; 4],
    zero: [u8; 8],
}

#[repr(C)]
struct SockaddrIn6 {
    family: u16,
    port: u16,
    flowinfo: u32,
    addr: [u8; 16],
    scope_id: u32,
}

#[repr(C, align(8))]
struct SockaddrStorage([u8; 128]);

#[repr(C)]
struct AddrInfoW {
    flags: i32,
    family: i32,
    socktype: i32,
    protocol: i32,
    addrlen: usize,
    canonname: *mut u16,
    addr: *mut SockaddrStorage,
    next: *mut AddrInfoW,
}

#[repr(C)]
struct WsaPollFd {
    fd: SOCKET,
    events: i16,
    revents: i16,
}

/// `WSAPROTOCOL_INFOW`, passed through opaquely.
#[repr(C)]
struct ProtocolInfo([u32; 157]);

#[repr(C)]
struct Linger {
    onoff: u16,
    secs: u16,
}

#[repr(C)]
struct IpMreq {
    multiaddr: u32,
    interface: u32,
}

unsafe extern "system" {
    fn WSAStartup(version: u16, data: *mut u8) -> i32;
    fn WSAGetLastError() -> i32;
    fn WSASocketW(
        af: i32,
        ty: i32,
        proto: i32,
        info: *mut ProtocolInfo,
        g: u32,
        flags: u32,
    ) -> SOCKET;
    fn WSADuplicateSocketW(s: SOCKET, pid: u32, info: *mut ProtocolInfo) -> i32;
    fn WSAIoctl(
        s: SOCKET,
        code: u32,
        input: *const c_void,
        in_len: u32,
        output: *mut c_void,
        out_len: u32,
        returned: *mut u32,
        ov: *mut c_void,
        routine: *mut c_void,
    ) -> i32;
    fn WSAPoll(fds: *mut WsaPollFd, n: u32, ms: i32) -> i32;
    fn closesocket(s: SOCKET) -> i32;
    fn ioctlsocket(s: SOCKET, cmd: i32, arg: *mut u32) -> i32;
    fn bind(s: SOCKET, addr: *const SockaddrStorage, len: i32) -> i32;
    fn listen(s: SOCKET, backlog: i32) -> i32;
    fn connect(s: SOCKET, addr: *const SockaddrStorage, len: i32) -> i32;
    fn accept(s: SOCKET, addr: *mut SockaddrStorage, len: *mut i32) -> SOCKET;
    fn getsockopt(s: SOCKET, level: i32, name: i32, v: *mut c_void, len: *mut i32) -> i32;
    fn setsockopt(s: SOCKET, level: i32, name: i32, v: *const c_void, len: i32) -> i32;
    fn getsockname(s: SOCKET, addr: *mut SockaddrStorage, len: *mut i32) -> i32;
    fn getpeername(s: SOCKET, addr: *mut SockaddrStorage, len: *mut i32) -> i32;
    #[link_name = "send"]
    fn wsa_send(s: SOCKET, buf: *const u8, len: i32, flags: i32) -> i32;
    fn sendto(
        s: SOCKET,
        buf: *const u8,
        len: i32,
        flags: i32,
        to: *const SockaddrStorage,
        tolen: i32,
    ) -> i32;
    #[link_name = "recv"]
    fn wsa_recv(s: SOCKET, buf: *mut u8, len: i32, flags: i32) -> i32;
    fn recvfrom(
        s: SOCKET,
        buf: *mut u8,
        len: i32,
        flags: i32,
        from: *mut SockaddrStorage,
        fromlen: *mut i32,
    ) -> i32;
    fn shutdown(s: SOCKET, how: i32) -> i32;
    fn GetAddrInfoW(
        node: *const u16,
        service: *const u16,
        hints: *const AddrInfoW,
        res: *mut *mut AddrInfoW,
    ) -> i32;
    fn FreeAddrInfoW(res: *mut AddrInfoW);
}

/// `-WSAGetLastError()`.
fn wsa_fail() -> i32 {
    // SAFETY: no preconditions.
    -unsafe { WSAGetLastError() }
}

/// Winsock 2.2, once per process.
fn startup() {
    static UP: AtomicBool = AtomicBool::new(false);
    if !UP.swap(true, Ordering::AcqRel) {
        let mut data = [0u8; 512];
        // SAFETY: `data` is larger than `WSADATA`.
        unsafe { WSAStartup(0x0202, data.as_mut_ptr()) };
    }
}

/// Word `i` of the `GcBuf<i64>` `buf`.
///
/// # Safety
/// `buf` must be a live `GcBuf<i64>` with more than `i` elements.
#[allow(clippy::cast_ptr_alignment)] // GC payloads are 8-aligned
const unsafe fn word(buf: *mut u8, i: usize) -> *mut i64 {
    // SAFETY: caller contract.
    unsafe { buf.add(HEADER_SIZE).cast::<i64>().add(i) }
}

/// The address in `buf` as a sockaddr and its length.
fn read_addr(buf: *mut u8) -> (SockaddrStorage, i32) {
    // SAFETY: the library passes a live `ADDR_WORDS` buffer; the storage
    // is large enough for either family.
    unsafe {
        let w = |i| *word(buf, i);
        let mut ss: SockaddrStorage = zeroed();
        let port = (w(1) as u16).to_be();
        if w(0) == 6 {
            let a = (&raw mut ss).cast::<SockaddrIn6>();
            (*a).family = AF_INET6 as u16;
            (*a).port = port;
            (*a).flowinfo = w(2) as u32;
            (*a).scope_id = w(3) as u32;
            for i in 0..16 {
                (*a).addr[i] = w(4 + i) as u8;
            }
            (ss, size_of::<SockaddrIn6>() as i32)
        } else {
            let a = (&raw mut ss).cast::<SockaddrIn>();
            (*a).family = AF_INET as u16;
            (*a).port = port;
            for i in 0..4 {
                (*a).addr[i] = w(4 + i) as u8;
            }
            (ss, size_of::<SockaddrIn>() as i32)
        }
    }
}

/// Store the address at `sa` into `buf` (no-op for a null `buf`).
#[allow(clippy::cast_ptr_alignment)] // a sockaddr of its own family
fn write_addr(buf: *mut u8, sa: *const SockaddrStorage) {
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
        if i32::from(*sa.cast::<u16>()) == AF_INET6 {
            let a = sa.cast::<SockaddrIn6>();
            put(0, 6);
            put(1, i64::from(u16::from_be((*a).port)));
            put(2, i64::from((*a).flowinfo));
            put(3, i64::from((*a).scope_id));
            for i in 0..16 {
                put(4 + i, i64::from((*a).addr[i]));
            }
        } else {
            let a = sa.cast::<SockaddrIn>();
            put(0, 4);
            put(1, i64::from(u16::from_be((*a).port)));
            for i in 0..4 {
                put(4 + i, i64::from((*a).addr[i]));
            }
        }
    }
}

/// Non-blocking, not inherited by children.
fn prepare(s: SOCKET) -> i32 {
    let mut on = 1u32;
    // SAFETY: plain calls on a socket we own.
    unsafe {
        if ioctlsocket(s, FIONBIO, &mut on) != 0 {
            return wsa_fail();
        }
        win::SetHandleInformation(s as HANDLE, win::HANDLE_FLAG_INHERIT, 0);
    }
    0
}

/// `s`, prepared, as an fd (or the error, `s` closed).
fn adopt(s: SOCKET) -> i32 {
    if s == win::INVALID_SOCKET {
        return wsa_fail();
    }
    let r = prepare(s);
    if r < 0 {
        // SAFETY: ours.
        unsafe { closesocket(s) };
        return r;
    }
    fd_of(s)
}

/// A new non-blocking socket: `family` 4 / 6, `kind` 1 stream / 2
/// datagram / 3 raw, `proto` an IP protocol number (0: the default for
/// `kind`). The fd or `-error`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_socket(family: i32, kind: i32, proto: i32) -> i32 {
    startup();
    let af = if family == 6 { AF_INET6 } else { AF_INET };
    let ty = match kind {
        2 => SOCK_DGRAM,
        3 => SOCK_RAW,
        _ => SOCK_STREAM,
    };
    // SAFETY: plain call.
    adopt(unsafe {
        WSASocketW(
            af,
            ty,
            proto,
            ptr::null_mut(),
            0,
            WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
        )
    })
}

/// The socket of `fd` (`fd` comes from this module).
fn sock(fd: i32) -> SOCKET {
    sock_of(fd).unwrap_or(win::INVALID_SOCKET)
}

/// 0, or `-error` for a Winsock `SOCKET_ERROR` result.
fn ok(r: i32) -> i32 {
    if r == SOCKET_ERROR { wsa_fail() } else { 0 }
}

/// `bind`. Windows lets a listener rebind over `TIME_WAIT` without
/// `SO_REUSEADDR` (which there would let another socket steal the port),
/// so a listener does not set it here.
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_bind(fd: i32, addr: *mut u8) -> i32 {
    let (ss, len) = read_addr(addr);
    // SAFETY: a valid sockaddr.
    ok(unsafe { bind(sock(fd), &ss, len) })
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_net_listen(fd: i32, backlog: i32) -> i32 {
    // SAFETY: plain call.
    ok(unsafe { listen(sock(fd), backlog) })
}

/// Start a connect: 0 when connected, 1 while pending (wait for
/// writability, then [`rok_net_sock_error`]), or `-error`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_connect(fd: i32, addr: *mut u8) -> i32 {
    let (ss, len) = read_addr(addr);
    // SAFETY: a valid sockaddr.
    if unsafe { connect(sock(fd), &ss, len) } == 0 {
        return 0;
    }
    match wsa_fail() {
        e if e == -win::WSAEWOULDBLOCK || e == -win::WSAEINPROGRESS => 1,
        e => e,
    }
}

/// The pending error of `fd` (`SO_ERROR`): 0 or `-error`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_sock_error(fd: i32) -> i32 {
    let mut e = 0i32;
    let mut n = 4;
    // SAFETY: valid out-pointers.
    let r = unsafe { getsockopt(sock(fd), SOL_SOCKET, SO_ERROR, (&raw mut e).cast(), &mut n) };
    if r != 0 { wsa_fail() } else { -e }
}

/// Accept one connection (non-blocking: `-WSAEWOULDBLOCK` if none is
/// queued); its peer goes to `out`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_accept(fd: i32, out: *mut u8) -> i32 {
    // SAFETY: zeroed storage is a valid out-buffer.
    let mut ss: SockaddrStorage = unsafe { zeroed() };
    let mut len = size_of::<SockaddrStorage>() as i32;
    // SAFETY: valid out-pointers.
    let c = adopt(unsafe { accept(sock(fd), &mut ss, &mut len) });
    if c >= 0 {
        write_addr(out, &ss);
    }
    c
}

/// The local (`peer == 0`) or peer address of `fd` into `out`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_addr(fd: i32, out: *mut u8, peer: i32) -> i32 {
    // SAFETY: zeroed storage is a valid out-buffer.
    let mut ss: SockaddrStorage = unsafe { zeroed() };
    let mut len = size_of::<SockaddrStorage>() as i32;
    // SAFETY: valid out-pointers.
    let r = unsafe {
        if peer == 0 {
            getsockname(sock(fd), &mut ss, &mut len)
        } else {
            getpeername(sock(fd), &mut ss, &mut len)
        }
    };
    if r != 0 {
        return wsa_fail();
    }
    write_addr(out, &ss);
    0
}

/// A Winsock byte count as `>= 0` or `-error`.
fn count(n: i32) -> isize {
    if n == SOCKET_ERROR {
        wsa_fail() as isize
    } else {
        n as isize
    }
}

/// One `recv` into `dst` (`len` bytes): the count or `-error`. After
/// `shutdown(Read)` it is the end of stream, as on Unix (Winsock fails
/// with `WSAESHUTDOWN`).
pub fn recv(s: SOCKET, dst: *mut u8, len: usize) -> isize {
    // SAFETY: the caller passes a writable region of `len` bytes.
    match count(unsafe { wsa_recv(s, dst, len.min(i32::MAX as usize) as i32, 0) }) {
        n if n == -(win::WSAESHUTDOWN as isize) => 0,
        n => n,
    }
}

/// One `send` of `src` (`len` bytes): the count or `-error`.
pub fn send(s: SOCKET, src: *const u8, len: usize) -> isize {
    // SAFETY: the caller passes a readable region of `len` bytes.
    count(unsafe { wsa_send(s, src, len.min(i32::MAX as usize) as i32, 0) })
}

/// One `send` of `buf[off..off + len]`: the count or `-error`.
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_net_send(fd: i32, buf: *mut u8, off: usize, len: usize) -> isize {
    // SAFETY: caller contract.
    send(sock(fd), unsafe { region(buf, off) }, len)
}

/// Listening sockets the host handed in: none on Windows.
#[unsafe(no_mangle)]
pub const extern "C" fn rok_net_listeners(_out: *mut u8, _max: usize) -> i32 {
    0
}

/// One `recv` into `buf[off..off + len]`: the count (0 at the end) or
/// `-error`.
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_net_recv(fd: i32, buf: *mut u8, off: usize, len: usize) -> isize {
    // SAFETY: caller contract.
    recv(sock(fd), unsafe { region(buf, off) }, len)
}

/// One datagram `buf[off..off + len]` to `addr`: the count or `-error`.
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
    // SAFETY: caller contract; a valid sockaddr.
    count(unsafe {
        sendto(
            sock(fd),
            region(buf, off),
            len.min(i32::MAX as usize) as i32,
            0,
            &ss,
            sl,
        )
    })
}

/// One datagram into `buf[off..off + len]` (truncated to fit); the
/// sender goes to `out` (may be null). The count or `-error`.
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
    // SAFETY: zeroed storage is a valid out-buffer.
    let mut ss: SockaddrStorage = unsafe { zeroed() };
    let mut sl = size_of::<SockaddrStorage>() as i32;
    // SAFETY: caller contract; valid out-pointers.
    let n = count(unsafe {
        recvfrom(
            sock(fd),
            region(buf, off),
            len.min(i32::MAX as usize) as i32,
            0,
            &mut ss,
            &mut sl,
        )
    });
    // Windows reports a datagram larger than the buffer as WSAEMSGSIZE
    // after filling it: that is the POSIX truncation.
    let n = if n == -(WSAEMSGSIZE as isize) {
        len as isize
    } else {
        n
    };
    if n >= 0 && sl > 0 {
        write_addr(out, &ss);
    }
    n
}

/// `shutdown`: `how` 0 read, 1 write, 2 both (the Winsock numbering).
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_shutdown(fd: i32, how: i32) -> i32 {
    // SAFETY: plain call.
    ok(unsafe { shutdown(sock(fd), how.clamp(0, 2)) })
}

fn setopt(fd: i32, level: i32, name: i32, v: i32) -> i32 {
    // SAFETY: a valid 4-byte option.
    ok(unsafe { setsockopt(sock(fd), level, name, (&raw const v).cast(), 4) })
}

fn getopt(fd: i32, level: i32, name: i32) -> Result<i32, i32> {
    let mut v = 0i32;
    let mut n = 4;
    // SAFETY: valid out-pointers.
    let r = unsafe { getsockopt(sock(fd), level, name, (&raw mut v).cast(), &mut n) };
    if r != 0 { Err(wsa_fail()) } else { Ok(v) }
}

/// Socket options by library code: 1 `TCP_NODELAY`, 2 keepalive (`val`
/// idle seconds, < 0 off), 3 `SO_BROADCAST`, 4 `IP_MULTICAST_LOOP`, 5
/// `IP_MULTICAST_TTL`, 7 `SO_REUSEADDR`, 9 `SO_RCVBUF`, 10 `SO_SNDBUF`, 11
/// `IP_TTL`, 12 `IPV6_V6ONLY`, 13 `SO_LINGER` (`val` seconds, < 0 off); 6
/// `UDP_SEGMENT` is Linux only, 8 `SO_REUSEPORT` Unix only.
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_setopt(fd: i32, opt: i32, val: i64) -> i32 {
    let v = val as i32;
    match opt {
        1 => setopt(fd, IPPROTO_TCP, TCP_NODELAY, v),
        2 => {
            let r = setopt(fd, SOL_SOCKET, SO_KEEPALIVE, i32::from(v >= 0));
            if r < 0 || v < 0 {
                return r;
            }
            setopt(fd, IPPROTO_TCP, TCP_KEEPIDLE, v.max(1))
        }
        3 => setopt(fd, SOL_SOCKET, SO_BROADCAST, v),
        4 => setopt(fd, IPPROTO_IP, IP_MULTICAST_LOOP, v),
        5 => setopt(fd, IPPROTO_IP, IP_MULTICAST_TTL, v),
        7 => setopt(fd, SOL_SOCKET, SO_REUSEADDR, v),
        9 => setopt(fd, SOL_SOCKET, SO_RCVBUF, v),
        10 => setopt(fd, SOL_SOCKET, SO_SNDBUF, v),
        11 => setopt(fd, IPPROTO_IP, IP_TTL, v),
        12 => setopt(fd, IPPROTO_IPV6, IPV6_V6ONLY, v),
        13 => {
            let l = Linger {
                onoff: u16::from(v >= 0),
                secs: v.clamp(0, i32::from(u16::MAX)) as u16,
            };
            // SAFETY: a valid option.
            ok(unsafe {
                setsockopt(
                    sock(fd),
                    SOL_SOCKET,
                    SO_LINGER,
                    (&raw const l).cast(),
                    size_of::<Linger>() as i32,
                )
            })
        }
        _ => -win::WSAEOPNOTSUPP,
    }
}

/// The value of option `opt` (as in [`rok_net_setopt`]) into word 0 of
/// the `GcBuf<i64>` `out`: 0 / 1 for a flag, a size, a TTL, or the linger
/// seconds (-1: off). 0 or `-error`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_getopt(fd: i32, opt: i32, out: *mut u8) -> i32 {
    let v = match opt {
        1 => getopt(fd, IPPROTO_TCP, TCP_NODELAY),
        3 => getopt(fd, SOL_SOCKET, SO_BROADCAST),
        7 => getopt(fd, SOL_SOCKET, SO_REUSEADDR),
        9 => getopt(fd, SOL_SOCKET, SO_RCVBUF),
        10 => getopt(fd, SOL_SOCKET, SO_SNDBUF),
        11 => getopt(fd, IPPROTO_IP, IP_TTL),
        12 => getopt(fd, IPPROTO_IPV6, IPV6_V6ONLY),
        13 => {
            let mut l = Linger { onoff: 0, secs: 0 };
            let mut n = size_of::<Linger>() as i32;
            // SAFETY: valid out-pointers.
            let r =
                unsafe { getsockopt(sock(fd), SOL_SOCKET, SO_LINGER, (&raw mut l).cast(), &mut n) };
            if r != 0 {
                Err(wsa_fail())
            } else if l.onoff == 0 {
                Ok(-1)
            } else {
                Ok(i32::from(l.secs))
            }
        }
        _ => Err(-win::WSAEOPNOTSUPP),
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

/// Join (`join != 0`) or leave IPv4 multicast `group` on `iface` (both
/// as `a.b.c.d` octets in a big-endian `u32`).
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_multicast_v4(fd: i32, group: u32, iface: u32, join: i32) -> i32 {
    let m = IpMreq {
        multiaddr: group.to_be(),
        interface: iface.to_be(),
    };
    let name = if join != 0 {
        IP_ADD_MEMBERSHIP
    } else {
        IP_DROP_MEMBERSHIP
    };
    // SAFETY: a valid option.
    ok(unsafe {
        setsockopt(
            sock(fd),
            IPPROTO_IP,
            name,
            (&raw const m).cast(),
            size_of::<IpMreq>() as i32,
        )
    })
}

/// Block (parked for the GC) until `fd` is readable (`write == 0`) or
/// writable, at most `timeout_ns` (< 0: no limit). 1 ready, 0 timed out,
/// or `-error`. Hang-ups and errors count as ready.
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_wait(fd: i32, write: i32, timeout_ns: i64) -> i32 {
    let mut p = WsaPollFd {
        fd: sock(fd),
        events: if write != 0 { POLLWRNORM } else { POLLRDNORM },
        revents: 0,
    };
    let ms = if timeout_ns < 0 {
        -1
    } else {
        timeout_ns.saturating_add(999_999) / 1_000_000
    }
    .min(i64::from(i32::MAX)) as i32;
    // SAFETY: one valid entry.
    let poll = |p: &mut WsaPollFd| unsafe { WSAPoll(p, 1, ms) };
    let r = if ms == 0 {
        poll(&mut p)
    } else {
        native(|| poll(&mut p)).0
    };
    if r < 0 {
        return wsa_fail();
    }
    i32::from(r > 0 && p.revents & (p.events | POLLERR | POLLHUP) != 0)
}

/// Resolve `host` (a name or numeric address) with `port`: up to `max`
/// addresses into `out` (`max * ADDR_WORDS` words), stream sockets only.
/// The count or `-error` (a Winsock code).
#[unsafe(no_mangle)]
pub extern "C" fn rok_net_resolve(host: RokStr, port: u16, out: *mut u8, max: usize) -> i32 {
    startup();
    // SAFETY: zeroed hints are valid.
    let mut hints: AddrInfoW = unsafe { zeroed() };
    hints.socktype = SOCK_STREAM;
    let mut res: *mut AddrInfoW = ptr::null_mut();
    let mut name = sys::wide(host.bytes());
    // SAFETY: a NUL-terminated name; valid hints / out-pointer.
    let r = native(|| unsafe { GetAddrInfoW(name.ptr(), ptr::null(), &hints, &mut res) }).0;
    if r != 0 {
        return -r;
    }
    let mut n = 0;
    let mut p = res;
    while !p.is_null() && n < max {
        // SAFETY: a list from GetAddrInfoW; `out` has room for `max`.
        unsafe {
            let fam = (*p).family;
            if fam == AF_INET || fam == AF_INET6 {
                let slot = out.add(n * ADDR_WORDS * 8);
                write_addr(slot, (*p).addr);
                *word(slot, 1) = i64::from(port);
                n += 1;
            }
            p = (*p).next;
        }
    }
    // SAFETY: from GetAddrInfoW.
    unsafe { FreeAddrInfoW(res) };
    n as i32
}

/// A second socket for the same connection (`os::rok_os_dup`).
pub fn dup_socket(s: SOCKET) -> i32 {
    // SAFETY: zeroed info is a valid out-buffer.
    let mut info: ProtocolInfo = unsafe { zeroed() };
    // SAFETY: valid out-pointer; duplicates into this process.
    if unsafe { WSADuplicateSocketW(s, win::GetCurrentProcessId(), &mut info) } != 0 {
        return wsa_fail();
    }
    // SAFETY: `info` describes the socket; -1 = take it from `info`.
    adopt(unsafe {
        WSASocketW(
            -1,
            -1,
            -1,
            &mut info,
            0,
            WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
        )
    })
}

/// Close socket `s` (`os::rok_os_close`), forgetting its watch first.
pub fn close_socket(s: SOCKET) -> i32 {
    forget(fd_of(s));
    // SAFETY: the library's own socket, closed once.
    ok(unsafe { closesocket(s) })
}

// The I/O driver's poller (707 §7.7.1): AFD polls on a completion port

const IOCTL_AFD_POLL: u32 = 0x0001_2024;
const AFD_POLL_RECEIVE: u32 = 0x1;
const AFD_POLL_RECEIVE_EXPEDITED: u32 = 0x2;
const AFD_POLL_SEND: u32 = 0x4;
const AFD_POLL_DISCONNECT: u32 = 0x8;
const AFD_POLL_ABORT: u32 = 0x10;
const AFD_POLL_LOCAL_CLOSE: u32 = 0x20;
const AFD_POLL_ACCEPT: u32 = 0x80;
const AFD_POLL_CONNECT_FAIL: u32 = 0x100;
/// Events that make a socket readable / writable (errors wake both).
const READ_EVENTS: u32 = AFD_POLL_RECEIVE
    | AFD_POLL_RECEIVE_EXPEDITED
    | AFD_POLL_DISCONNECT
    | AFD_POLL_ACCEPT
    | AFD_POLL_ABORT
    | AFD_POLL_CONNECT_FAIL;
const WRITE_EVENTS: u32 = AFD_POLL_SEND | AFD_POLL_ABORT | AFD_POLL_CONNECT_FAIL;
const READ: u8 = 1;
const WRITE: u8 = 2;

#[repr(C)]
struct AfdPollInfo {
    timeout: i64,
    count: u32,
    exclusive: u32,
    handle: HANDLE,
    events: u32,
    status: i32,
}

/// One poll request in flight: freed when its completion is dequeued.
#[repr(C)]
struct Op {
    iosb: win::IO_STATUS_BLOCK,
    info: AfdPollInfo,
    /// The watched fd.
    fd: i32,
    /// The directions it waits for.
    dirs: u8,
}

/// A watched socket.
struct Rec {
    next: *mut Rec,
    fd: i32,
    /// `SIO_BASE_HANDLE` of the socket: what AFD polls.
    base: SOCKET,
    /// Directions to report (cleared once reported, set by a watch).
    armed: u8,
    /// The request in flight for exactly this record, or null.
    op: *mut Op,
}

const BUCKETS: usize = 1024;

/// The driver's state: the record table (chained by fd), under `lock`.
struct Driver {
    lock: sys::Lock,
    table: UnsafeCell<[*mut Rec; BUCKETS]>,
}

// SAFETY: `table` and everything it reaches is only touched with `lock`
// held.
unsafe impl Sync for Driver {}

static DRIVER: Driver = Driver {
    lock: sys::Lock::new(),
    table: UnsafeCell::new([ptr::null_mut(); BUCKETS]),
};

/// The completion port and the `\Device\Afd` handle bound to it.
static PORT: AtomicIsize = AtomicIsize::new(0);
static AFD: AtomicIsize = AtomicIsize::new(0);

/// With the driver lock held: the link holding `fd`'s record (or the
/// null link at its chain's end).
fn link(fd: i32) -> *mut *mut Rec {
    // SAFETY: the caller holds the lock.
    unsafe {
        let mut l = (*DRIVER.table.get())
            .as_mut_ptr()
            .add(fd as u32 as usize / 4 % BUCKETS);
        while !(*l).is_null() && (**l).fd != fd {
            l = &raw mut (**l).next;
        }
        l
    }
}

/// The process's completion port, created (with the AFD handle) on
/// first use.
#[unsafe(no_mangle)]
pub extern "C" fn rok_io_poller() -> i32 {
    let p = PORT.load(Ordering::Acquire);
    if p != 0 {
        return p as i32;
    }
    DRIVER.lock.lock();
    if PORT.load(Ordering::Relaxed) == 0 {
        startup();
        let name = crate::os::ascii_w(br"\Device\Afd\Rok");
        let us = win::UNICODE_STRING {
            Length: (name.len() * 2) as u16,
            MaximumLength: (name.len() * 2) as u16,
            Buffer: name.as_ptr(),
        };
        let oa = win::OBJECT_ATTRIBUTES {
            Length: size_of::<win::OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: 0,
            ObjectName: &us,
            Attributes: 0,
            SecurityDescriptor: ptr::null_mut(),
            SecurityQualityOfService: ptr::null_mut(),
        };
        let mut afd = 0;
        // SAFETY: zeroed status block; valid attributes and out-pointers.
        unsafe {
            let mut iosb: win::IO_STATUS_BLOCK = zeroed();
            let port = win::CreateIoCompletionPort(win::INVALID_HANDLE_VALUE, 0, 0, 1);
            let s = win::NtCreateFile(
                &mut afd,
                win::SYNCHRONIZE,
                &oa,
                &mut iosb,
                ptr::null(),
                0,
                win::FILE_SHARE_READ | win::FILE_SHARE_WRITE,
                win::FILE_OPEN,
                0,
                ptr::null_mut(),
                0,
            );
            if port == 0 || s < 0 || win::CreateIoCompletionPort(afd, port, 0, 0) == 0 {
                rokr_common::fatal(b"rok: cannot create the I/O poller");
            }
            AFD.store(afd, Ordering::Relaxed);
            PORT.store(port, Ordering::Release);
        }
    }
    DRIVER.lock.unlock();
    PORT.load(Ordering::Acquire) as i32
}

/// With the driver lock held: make sure a request in flight covers every
/// armed direction of `r` (a narrower one is cancelled and replaced). 0
/// or `-error`.
fn arm(r: *mut Rec) -> i32 {
    // SAFETY: the caller holds the lock; `r` is a live record.
    unsafe {
        let want = (*r).armed;
        let op = (*r).op;
        if want == 0 || (!op.is_null() && (*op).dirs & want == want) {
            return 0;
        }
        if !op.is_null() {
            // Its completion (cancelled or not) finds `r.op` changed and
            // is dropped.
            win::CancelIoEx(AFD.load(Ordering::Relaxed), (&raw mut (*op).iosb).cast());
        }
        let new = libc::malloc(size_of::<Op>()).cast::<Op>();
        if new.is_null() {
            rokr_common::oom();
        }
        let mut events = AFD_POLL_LOCAL_CLOSE;
        if want & READ != 0 {
            events |= READ_EVENTS;
        }
        if want & WRITE != 0 {
            events |= WRITE_EVENTS;
        }
        new.write(Op {
            iosb: zeroed(),
            info: AfdPollInfo {
                timeout: i64::MAX,
                count: 1,
                exclusive: 0,
                handle: (*r).base as HANDLE,
                events,
                status: 0,
            },
            fd: (*r).fd,
            dirs: want,
        });
        let info = (&raw mut (*new).info).cast::<c_void>();
        let n = size_of::<AfdPollInfo>() as u32;
        let s = win::NtDeviceIoControlFile(
            AFD.load(Ordering::Relaxed),
            0,
            ptr::null_mut(),
            new.cast(),
            &raw mut (*new).iosb,
            IOCTL_AFD_POLL,
            info,
            n,
            info,
            n,
        );
        if s < 0 {
            libc::free(new.cast());
            (*r).op = ptr::null_mut();
            return -(win::RtlNtStatusToDosError(s) as i32);
        }
        // Pending or done at once: either way the port gets the completion.
        (*r).op = new;
    }
    0
}

/// Watch `fd` for readability and writability, edge-triggered (one event
/// per readiness after each watch, so a waiter re-checks after it
/// parks). Watching again re-arms. 0 or `-error`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_io_watch(fd: i32, _interest: u32) -> i32 {
    rok_io_poller();
    let Some(s) = sock_of(fd) else {
        return -win::WSAENOTSOCK;
    };
    DRIVER.lock.lock();
    let l = link(fd);
    // SAFETY: the lock is held; records are ours.
    let r = unsafe {
        if (*l).is_null() {
            let mut base: SOCKET = 0;
            let mut got = 0;
            let rc = WSAIoctl(
                s,
                SIO_BASE_HANDLE,
                ptr::null(),
                0,
                (&raw mut base).cast(),
                size_of::<SOCKET>() as u32,
                &mut got,
                ptr::null_mut(),
                ptr::null_mut(),
            );
            if rc != 0 {
                let e = wsa_fail();
                DRIVER.lock.unlock();
                return e;
            }
            let r = libc::malloc(size_of::<Rec>()).cast::<Rec>();
            if r.is_null() {
                rokr_common::oom();
            }
            r.write(Rec {
                next: ptr::null_mut(),
                fd,
                base,
                armed: 0,
                op: ptr::null_mut(),
            });
            *l = r;
        }
        (**l).armed = READ | WRITE;
        arm(*l)
    };
    DRIVER.lock.unlock();
    r
}

/// For a REPL host about to put back the memory of before a failed cell,
/// every other thread suspended: cancel the requests in flight and take
/// their completions, which would otherwise land in memory the restored
/// session does not own. No lock: a suspended thread may hold it.
pub fn quiesce() {
    let (port, afd) = (PORT.load(Ordering::Acquire), AFD.load(Ordering::Acquire));
    if port == 0 {
        return;
    }
    // SAFETY: our handles; a cancel of nothing is harmless.
    unsafe { win::CancelIoEx(afd, ptr::null_mut()) };
    // SAFETY: zeroed entries are a valid out-buffer.
    let mut evs: [win::OVERLAPPED_ENTRY; 64] = unsafe { zeroed() };
    let mut got = 0u32;
    // A cancelled request completes promptly: drain until the port stays
    // empty for a while.
    // SAFETY: valid buffer and out-pointer.
    while unsafe { win::GetQueuedCompletionStatusEx(port, evs.as_mut_ptr(), 64, &mut got, 50, 0) }
        != 0
        && got > 0
    {}
}

/// After [`quiesce`] and the restore: the records' requests are gone
/// (waiters watch their sockets again when woken).
pub fn reset_poller() {
    if PORT.load(Ordering::Acquire) == 0 {
        return;
    }
    DRIVER.lock.lock();
    // SAFETY: the lock is held; records are ours.
    unsafe {
        for head in &*DRIVER.table.get() {
            let mut r = *head;
            while !r.is_null() {
                (*r).op = ptr::null_mut();
                (*r).armed = 0;
                r = (*r).next;
            }
        }
    }
    DRIVER.lock.unlock();
}

/// Drop `fd`'s record (its socket is closing). A request in flight
/// completes later and is dropped.
fn forget(fd: i32) {
    if PORT.load(Ordering::Acquire) == 0 {
        return;
    }
    DRIVER.lock.lock();
    let l = link(fd);
    // SAFETY: the lock is held; the record is ours.
    unsafe {
        let r = *l;
        if !r.is_null() {
            *l = (*r).next;
            if !(*r).op.is_null() {
                win::CancelIoEx(
                    AFD.load(Ordering::Relaxed),
                    (&raw mut (*(*r).op).iosb).cast(),
                );
            }
            libc::free(r.cast());
        }
    }
    DRIVER.lock.unlock();
}

/// Block (parked for the GC) until watched sockets change; up to `max`
/// events go to the `GcBuf<i64>` `out` as `fd * 2 + (0 read | 1 write)`.
/// The count, or `-error`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_io_poll(out: *mut u8, max: usize) -> i32 {
    const N: usize = 64;
    let port = rok_io_poller() as HANDLE;
    // SAFETY: zeroed entries are a valid out-buffer.
    let mut evs: [win::OVERLAPPED_ENTRY; N] = unsafe { zeroed() };
    let mut got = 0u32;
    // SAFETY: valid buffer; no timeout.
    let (ok, e) = native(|| unsafe {
        win::GetQueuedCompletionStatusEx(
            port,
            evs.as_mut_ptr(),
            N as u32,
            &mut got,
            win::INFINITE,
            0,
        )
    });
    if ok == 0 {
        return -e;
    }
    let mut keys = [0i64; 2 * N];
    let mut n = 0;
    DRIVER.lock.lock();
    for ev in &evs[..got as usize] {
        // An `Op` from `malloc`, aligned for it.
        #[allow(clippy::cast_ptr_alignment)]
        let op = ev.lpOverlapped.cast::<Op>();
        // SAFETY: every completion on the port is an `Op` of ours; the
        // lock is held.
        unsafe {
            let l = link((*op).fd);
            let r = *l;
            if !r.is_null() && (*r).op == op {
                (*r).op = ptr::null_mut();
                let status = (*op).iosb.Status;
                let events = if status < 0 || (*op).info.count == 0 {
                    0
                } else {
                    (*op).info.events
                };
                // A failed request (not a cancel) wakes what it waited for.
                let mut dirs = if status < 0 && status != win::STATUS_CANCELLED {
                    (*op).dirs
                } else {
                    0
                };
                if events & READ_EVENTS != 0 {
                    dirs |= READ;
                }
                if events & WRITE_EVENTS != 0 {
                    dirs |= WRITE;
                }
                dirs &= (*r).armed;
                for (bit, d) in [(READ, 0), (WRITE, 1)] {
                    if dirs & bit != 0 {
                        keys[n] = i64::from((*r).fd) * 2 + d;
                        n += 1;
                    }
                }
                (*r).armed &= !dirs;
                if events & AFD_POLL_LOCAL_CLOSE != 0 {
                    *l = (*r).next;
                    libc::free(r.cast());
                } else {
                    arm(r);
                }
            }
            libc::free(op.cast());
        }
    }
    DRIVER.lock.unlock();
    let n = n.min(max);
    for (i, k) in keys[..n].iter().enumerate() {
        // SAFETY: the library passes `max` words.
        unsafe { *word(out, i) = *k };
    }
    n as i32
}
