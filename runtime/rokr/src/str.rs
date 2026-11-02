//! Runtime side of `str`: a `(ptr, len)` value.
//!
//! Literals point at static NUL-terminated bytes. Every string built at
//! run time is a GC object (type id [`STR_TYPE_ID`], no traced children)
//! whose payload holds the bytes plus a trailing NUL. A slice shares its
//! source's bytes (`(ptr + start, end - start)`), so `ptr` may point
//! anywhere inside an object, or at its NUL for an empty tail: the
//! collector treats `ptr` as an interior pointer, which keeps the whole
//! object alive. A slice is not NUL-terminated in general; C gets
//! [`rok_str_cstr`].
//!
//! `rok_*` symbols take and return `str` by value: two words, the same
//! registers as the LLVM `{ptr, iN}` aggregate the compiler passes.
//! Allocating entries may collect; their `str` arguments stay alive
//! because the caller's shadow-stack frame roots them.

use core::ptr;

use rokr_common::object_header::{HEADER_ALIGN, HEADER_SIZE};

use crate::{str_ops, unicode};

/// Standard-library type id of runtime strings (in the reserved
/// range); never registered, so the collector traces nothing inside.
const STR_TYPE_ID: u32 = rokr_common::gc_abi::PLAIN_TYPE_ID;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RokStr {
    pub ptr: *const u8,
    pub len: usize,
}

impl RokStr {
    pub(crate) const fn bytes(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: a `str` value's `ptr` is valid for `len` bytes.
        unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
    }
}

/// A fresh GC string of `len` bytes (zeroed, NUL-terminated); returns
/// the writable byte pointer.
pub fn alloc_str(len: usize) -> *mut u8 {
    let Some(size) = len.checked_add(HEADER_SIZE + 1) else {
        rokr_common::fatal(b"rok runtime: string too long");
    };
    let obj = rokr_gc::alloc(size, HEADER_ALIGN, STR_TYPE_ID);
    // SAFETY: the payload follows the 16-byte header.
    unsafe { obj.add(HEADER_SIZE) }
}

/// GC string holding the concatenation of `parts`.
pub fn build(parts: &[&[u8]]) -> RokStr {
    let len = parts.iter().map(|p| p.len()).sum();
    let p = alloc_str(len);
    let mut at = 0;
    for part in parts {
        // SAFETY: `p` has room for `len` bytes; sources are distinct
        // (a fresh object never overlaps an argument).
        unsafe { ptr::copy_nonoverlapping(part.as_ptr(), p.add(at), part.len()) };
        at += part.len();
    }
    RokStr { ptr: p, len }
}

/// `strlen` for a `char*` a C function returned; null → 0.
///
/// # Safety
/// `s` must be null or a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_cstr_len(s: *const u8) -> usize {
    if s.is_null() {
        return 0;
    }
    // SAFETY: caller contract.
    unsafe { libc::strlen(s.cast()) }
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_str_concat(a: RokStr, b: RokStr) -> RokStr {
    build(&[a.bytes(), b.bytes()])
}

/// Byte-wise three-way compare: -1, 0 or 1.
#[unsafe(no_mangle)]
pub extern "C" fn rok_str_cmp(a: RokStr, b: RokStr) -> i32 {
    a.bytes().cmp(b.bytes()) as i32
}

/// 1 if `a` and `b` hold the same bytes, else 0 (`str`'s `==`).
#[unsafe(no_mangle)]
pub extern "C" fn rok_str_eq(a: RokStr, b: RokStr) -> i32 {
    i32::from(a.bytes() == b.bytes())
}

/// `s[start:end]` (byte offsets): shares `s`'s bytes, O(1). Panics
/// outside `0..=len`, on a reversed range, or off a UTF-8 char boundary,
/// reporting `at` (the caller's `path:line:col`).
#[unsafe(no_mangle)]
pub extern "C" fn rok_str_slice(s: RokStr, start: usize, end: usize, at: RokStr) -> RokStr {
    let b = s.bytes();
    if let Err(m) = str_ops::slice_check(b, start, end) {
        crate::fmt::panic_with(&m, Some(at.bytes()));
    }
    sub(&s, &b[start..end])
}

/// `part`, a subslice of `s.bytes()`, as a `str` sharing `s`'s bytes.
pub const fn sub(s: &RokStr, part: &[u8]) -> RokStr {
    if s.len == 0 {
        // `bytes()` of an empty `str` may not point into it.
        return *s;
    }
    RokStr {
        ptr: part.as_ptr(),
        len: part.len(),
    }
}

/// `s` as a C string: `s.ptr` itself when a NUL follows the bytes
/// (literals, whole runtime strings, tail slices), else a pinned GC copy
/// for [`rok_str_cstr_done`] to release. Compiled code brackets each
/// `extern "C"` call that takes a `str` with the two.
#[unsafe(no_mangle)]
pub extern "C" fn rok_str_cstr(s: RokStr) -> *const u8 {
    if s.len == 0 {
        return c"".as_ptr().cast();
    }
    // SAFETY: byte `len` of a `str` is readable: the next byte of its
    // source, or the NUL after it.
    if unsafe { *s.ptr.add(s.len) } == 0 {
        return s.ptr;
    }
    let c = build(&[s.bytes()]);
    // SAFETY: a fresh GC string; its object starts `HEADER_SIZE` before.
    unsafe { rokr_gc::pin(c.ptr.cast_mut().sub(HEADER_SIZE)) };
    c.ptr
}

/// Release `c`, what [`rok_str_cstr`] returned for `s`.
///
/// # Safety
/// `c` must come from `rok_str_cstr(s)` and be released once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_str_cstr_done(s: RokStr, c: *const u8) {
    if s.len != 0 && c != s.ptr {
        // SAFETY: caller contract: `c` is a pinned copy's payload.
        unsafe { rokr_gc::unpin(c.cast_mut().sub(HEADER_SIZE)) };
    }
}

/// `s`'s bytes as a `GcBuf<u8>` whose element 0 is byte 0 of `s`. The
/// word points `HEADER_SIZE` before the bytes: inside the object for a
/// runtime string (an interior reference), outside the heap for a
/// literal. Read-only: `str` is immutable, literals are in rodata.
#[unsafe(no_mangle)]
pub const extern "C" fn rok_str_bytes_buf(s: RokStr) -> *const u8 {
    s.ptr.wrapping_sub(HEADER_SIZE)
}

/// Byte offset of the first `pat` in `s`, or -1.
#[unsafe(no_mangle)]
pub extern "C" fn rok_str_find(s: RokStr, pat: RokStr) -> isize {
    str_ops::find(s.bytes(), pat.bytes()).map_or(-1, |i| i as isize)
}

/// Byte offset of the last `pat` in `s`, or -1.
#[unsafe(no_mangle)]
pub extern "C" fn rok_str_rfind(s: RokStr, pat: RokStr) -> isize {
    str_ops::rfind(s.bytes(), pat.bytes()).map_or(-1, |i| i as isize)
}

/// Every non-overlapping `from` replaced by `to` (`str_ops::replace`).
#[unsafe(no_mangle)]
#[allow(clippy::many_single_char_names)] // byte-crunching locals
pub extern "C" fn rok_str_replace(s: RokStr, from: RokStr, to: RokStr) -> RokStr {
    let (h, f, t) = (s.bytes(), from.bytes(), to.bytes());
    // Two passes: measure, then copy.
    let mut len = 0;
    str_ops::replace(h, f, t, |b| len += b.len());
    let p = alloc_str(len);
    let mut o = 0;
    str_ops::replace(h, f, t, |b| {
        // SAFETY: the pieces add up to the `len` bytes measured above.
        unsafe { ptr::copy_nonoverlapping(b.as_ptr(), p.add(o), b.len()) };
        o += b.len();
    });
    RokStr { ptr: p, len }
}

/// Number of chars (UTF-8 scalar values) in `s`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_str_char_count(s: RokStr) -> usize {
    s.bytes().iter().filter(|&&b| (b & 0xC0) != 0x80).count()
}

/// UTF-8 encoding of `c` (U+FFFD for a non-scalar value).
fn utf8(c: u32, buf: &mut [u8; 4]) -> &[u8] {
    char::from_u32(c)
        .unwrap_or(char::REPLACEMENT_CHARACTER)
        .encode_utf8(buf)
        .as_bytes()
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_str_from_char(c: u32) -> RokStr {
    let mut buf = [0u8; 4];
    build(&[utf8(c, &mut buf)])
}

/// Byte `i` of `s` (`bytes()`); the caller checks `i < len`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_str_byte(s: RokStr, i: usize) -> u8 {
    s.bytes().get(i).copied().unwrap_or(0)
}

/// `s` as UTF-8 text; "" when it is not (`rok_os_arg` / `rok_os_env_at`
/// hand raw OS bytes over as a `str`, which std checks later).
pub fn text(s: &RokStr) -> &str {
    core::str::from_utf8(s.bytes()).unwrap_or("")
}

/// `s` as an integer of `bits` bits in base `radix` (2..=36), signed
/// when `signed != 0`: an optional sign, then digits of either case. The
/// one integer grammar (`FromStr` and `Int::parse_radix`). The value's
/// bits go to `out[0]`; returns 0 ok, 1 empty, 2 invalid digit, 3 out of
/// range.
///
/// # Safety
/// `out` is a live `GcBuf<i64>` of at least one element.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_str_parse_int(
    s: RokStr,
    radix: u32,
    signed: i32,
    bits: u32,
    out: *mut u8,
) -> i32 {
    let v = match str_ops::parse_int(s.bytes(), radix, signed != 0, bits) {
        Ok(v) => v,
        Err(st) => return st,
    };
    // SAFETY: caller contract; elements start after the header.
    unsafe { ptr::copy_nonoverlapping(v.to_ne_bytes().as_ptr(), out.add(HEADER_SIZE), 8) };
    0
}

/// `s` as an `f64` (`inf`, `NaN` and exponents included) into `out[0]`,
/// a `GcBuf<f64>`; returns 0 ok, 2 invalid, 3 out of range.
///
/// # Safety
/// `out` is a live `GcBuf<f64>` of at least one element.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_str_parse_float(s: RokStr, out: *mut u8) -> i32 {
    let v = match str_ops::parse_float(text(&s)) {
        Ok(v) => v,
        Err(st) => return st,
    };
    // SAFETY: caller contract; elements start after the header.
    unsafe { ptr::copy_nonoverlapping(v.to_ne_bytes().as_ptr(), out.add(HEADER_SIZE), 8) };
    0
}

// Byte buffers (`GcBuf<u8>` regions: `StrBuf`, byte lists, I/O)

/// Address of `s`'s first byte (`str::as_ptr`).
#[unsafe(no_mangle)]
pub extern "C" fn rok_str_addr(s: RokStr) -> usize {
    s.ptr as usize
}

/// Byte length of `s` (`str::len`).
#[unsafe(no_mangle)]
pub const extern "C" fn rok_str_len(s: RokStr) -> usize {
    s.len
}

/// 1 if `s[at..at + pat.len]` is `pat`, else 0 (`starts_with` / `ends_with`).
#[unsafe(no_mangle)]
pub extern "C" fn rok_str_eq_at(s: RokStr, at: usize, pat: RokStr) -> i32 {
    i32::from(str_ops::eq_at(s.bytes(), at, pat.bytes()))
}

/// `len` bytes of the `GcBuf<u8>` `buf` from `off`, as a new `str`. No
/// UTF-8 check: the caller vouches for the bytes (`rok_buf_utf8_error`).
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_str_from_buf(buf: *mut u8, off: usize, len: usize) -> RokStr {
    // SAFETY: caller contract. `build` may collect before copying; the
    // caller's frame roots `buf`, so it stays in place.
    build(&[unsafe { core::slice::from_raw_parts(buf.add(HEADER_SIZE + off), len) }])
}

/// Copy `s` into the `GcBuf<u8>` `buf` at `off`.
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + s.len` bytes.
#[unsafe(no_mangle)]
pub const unsafe extern "C" fn rok_buf_put_str(buf: *mut u8, off: usize, s: RokStr) {
    if s.len > 0 {
        // SAFETY: caller contract.
        unsafe { ptr::copy_nonoverlapping(s.ptr, buf.add(HEADER_SIZE + off), s.len) };
    }
}

/// `c` UTF-8 encoded into the `GcBuf<u8>` `buf` at `off`; the byte count.
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + 4` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_buf_put_char(buf: *mut u8, off: usize, c: u32) -> usize {
    let mut tmp = [0u8; 4];
    let b = utf8(c, &mut tmp);
    // SAFETY: caller contract.
    unsafe { ptr::copy_nonoverlapping(b.as_ptr(), buf.add(HEADER_SIZE + off), b.len()) };
    b.len()
}

/// Copy `len` bytes from `src[soff..]` to `dst[doff..]` (may overlap).
///
/// # Safety
/// Both must be live `GcBuf<u8>`s holding the ranges.
#[unsafe(no_mangle)]
pub const unsafe extern "C" fn rok_buf_copy(
    dst: *mut u8,
    doff: usize,
    src: *mut u8,
    soff: usize,
    len: usize,
) {
    // SAFETY: caller contract.
    let (from, to) = unsafe { (src.add(HEADER_SIZE + soff), dst.add(HEADER_SIZE + doff)) };
    // SAFETY: as above.
    unsafe { ptr::copy(from, to, len) };
}

/// Offset of the first invalid UTF-8 byte of `buf[off..off + len]`, or -1.
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `off + len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_buf_utf8_error(buf: *mut u8, off: usize, len: usize) -> isize {
    // SAFETY: caller contract.
    let b = unsafe { core::slice::from_raw_parts(buf.add(HEADER_SIZE + off), len) };
    str_ops::utf8_error(b)
}

/// `buf[start..end]`, checked as `str` slicing over the `len` bytes of
/// contents (`StrBuf`), as a new `str`; panics like `rok_str_slice`.
///
/// # Safety
/// `buf` must be a live `GcBuf<u8>` with at least `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_buf_slice_str(
    buf: *mut u8,
    len: usize,
    start: usize,
    end: usize,
    at: RokStr,
) -> RokStr {
    // SAFETY: caller contract.
    let b = unsafe { core::slice::from_raw_parts(buf.add(HEADER_SIZE), len) };
    if let Err(m) = str_ops::slice_check(b, start, end) {
        crate::fmt::panic_with(&m, Some(at.bytes()));
    }
    // `build` may collect before copying; the caller's frame roots `buf`.
    build(&[&b[start..end]])
}

// Unicode (`unicode`): `char` properties, case, trimming

/// Property bits of `c`: 1 Alphabetic, 2 Lowercase, 4 Uppercase,
/// 8 `White_Space`, 16 numeric (general category Nd / Nl / No).
#[unsafe(no_mangle)]
pub extern "C" fn rok_char_props(c: u32) -> u32 {
    unicode::props(c)
}

/// Simple (one-char) case mapping, `mode` 0 lower, 1 upper, 2 title;
/// `c` itself when it has none.
#[unsafe(no_mangle)]
pub extern "C" fn rok_char_to_case(c: u32, mode: i32) -> u32 {
    unicode::to_case(c, mode)
}

/// Full case mapping of each char (`unicode::case`), `mode` as
/// `rok_char_to_case`; ASCII only when `s` is not UTF-8.
#[unsafe(no_mangle)]
pub extern "C" fn rok_str_case(s: RokStr, mode: i32) -> RokStr {
    let Ok(text) = core::str::from_utf8(s.bytes()) else {
        let out = build(&[s.bytes()]);
        // SAFETY: `out` is a fresh GC string of `len` bytes.
        let b = unsafe { core::slice::from_raw_parts_mut(out.ptr.cast_mut(), out.len) };
        if mode == unicode::LOWER {
            b.make_ascii_lowercase();
        } else {
            b.make_ascii_uppercase();
        }
        return out;
    };
    // Two passes: measure, then encode.
    let mut len = 0;
    unicode::case(text, mode, |m| len += m.len_utf8());
    let p = alloc_str(len);
    let mut at = 0;
    unicode::case(text, mode, |m| {
        let mut buf = [0u8; 4];
        let e = m.encode_utf8(&mut buf);
        // SAFETY: the first pass measured exactly `len` bytes.
        unsafe { ptr::copy_nonoverlapping(e.as_ptr(), p.add(at), e.len()) };
        at += e.len();
    });
    RokStr { ptr: p, len }
}

/// `which`: 0 = both ends, 1 = start, 2 = end (`unicode::trim`). Shares
/// `s`'s bytes.
#[unsafe(no_mangle)]
pub extern "C" fn rok_str_trim(s: RokStr, which: i32) -> RokStr {
    sub(&s, unicode::trim(s.bytes(), which))
}

#[cfg(test)]
pub mod tests {
    use super::*;

    const fn s(b: &'static [u8]) -> RokStr {
        RokStr {
            ptr: b.as_ptr(),
            len: b.len(),
        }
    }

    // Non-allocating entries: safe to run alongside other tests.
    #[test]
    fn non_allocating_ops() {
        assert_eq!(rok_str_char_count(s("a한😀".as_bytes())), 3);
        assert_eq!(rok_str_find(s(b"abc"), s(b"")), 0);
        assert_eq!(rok_str_find(s(b"abc"), s(b"x")), -1);
        assert_eq!(rok_str_rfind(s(b"abc"), s(b"")), 3);
        assert_eq!(rok_str_byte(s(b"ab"), 1), b'b');
        assert_eq!(rok_str_byte(s(b"ab"), 9), 0);
    }

    #[test]
    fn parse_numbers() {
        // A stand-in `GcBuf`: header bytes, then one 8-aligned element.
        #[repr(align(16))]
        struct Out([u8; HEADER_SIZE + 8]);
        let mut o = Out([0; HEADER_SIZE + 8]);
        let mut int = |v: &'static [u8], radix, signed, bits| {
            // SAFETY: `o` holds one element.
            let st = unsafe { rok_str_parse_int(s(v), radix, signed, bits, o.0.as_mut_ptr()) };
            let val = i64::from_ne_bytes(o.0[HEADER_SIZE..].try_into().unwrap_or_default());
            (st, val)
        };
        assert_eq!(int(b"-128", 10, 1, 8), (0, -128));
        assert_eq!(int(b"128", 10, 1, 8).0, 3);
        assert_eq!(int(b"255", 10, 0, 8), (0, 255));
        assert_eq!(int(b"256", 10, 0, 8).0, 3);
        assert_eq!(int(b"-1", 10, 0, 32).0, 3);
        assert_eq!(int(b"+7", 10, 1, 32), (0, 7));
        assert_eq!(int(b"", 10, 1, 32).0, 1);
        assert_eq!(int(b"-", 10, 1, 32).0, 1);
        assert_eq!(int(b"1_000", 10, 1, 32).0, 2);
        assert_eq!(int(b" 1", 10, 1, 32).0, 2);
        assert_eq!(int(b"fF", 16, 1, 32), (0, 255));
        assert_eq!(int(b"-9223372036854775808", 10, 1, 64), (0, i64::MIN));
        assert_eq!(int(b"18446744073709551615", 10, 0, 64), (0, -1));
        assert_eq!(int(b"18446744073709551616", 10, 0, 64).0, 3);
        let mut float = |v: &'static [u8]| {
            // SAFETY: `o` holds one element.
            let st = unsafe { rok_str_parse_float(s(v), o.0.as_mut_ptr()) };
            let val = f64::from_ne_bytes(o.0[HEADER_SIZE..].try_into().unwrap_or_default());
            (st, val)
        };
        assert_eq!(float(b"1e-3"), (0, 1e-3));
        assert_eq!(float(b"inf"), (0, f64::INFINITY));
        assert_eq!(float(b"1.2.3").0, 2);
        assert_eq!(float(b"1e400").0, 3);
        assert_eq!(float(b"-1e400").0, 3);
    }

    // Run from `gc_alloc_initializes_header_and_alignment`: the GC heap is
    // single-mutator, so allocating tests must not run in parallel.
    pub fn string_ops() {
        assert_eq!(rok_str_concat(s(b"ab"), s(b"cd")).bytes(), b"abcd");
        assert_eq!(rok_str_cmp(s(b"ab"), s(b"b")), -1);
        assert_eq!(rok_str_cmp(s(b"ab"), s(b"ab")), 0);
        assert_eq!(
            rok_str_slice(s("a세b".as_bytes()), 1, 4, s(b"")).bytes(),
            "세".as_bytes()
        );
        assert_eq!(rok_str_find(s(b"hello"), s(b"ll")), 2);
        assert_eq!(rok_str_rfind(s(b"abab"), s(b"ab")), 2);
        assert_eq!(rok_str_trim(s(b"  x "), 0).bytes(), b"x");
        assert_eq!(rok_str_case(s("straße".as_bytes()), 1).bytes(), b"STRASSE");
        assert_eq!(
            rok_str_case(s("ΟΔΟΣ Σ".as_bytes()), 0).bytes(),
            "οδος σ".as_bytes()
        );
        assert_eq!(
            rok_str_trim(s("\u{3000} x\u{85}".as_bytes()), 0).bytes(),
            b"x"
        );
        assert_eq!(rok_str_trim(s(b"\xff "), 2).bytes(), b"\xff");
        assert_eq!(
            rok_str_replace(s(b"a-b-c"), s(b"-"), s(b"+")).bytes(),
            b"a+b+c"
        );
        assert_eq!(rok_str_from_char(0xD55C).bytes(), "한".as_bytes());
        // Runtime strings stay NUL-terminated for C.
        let c = rok_str_concat(s(b"x"), s(b"y"));
        // SAFETY: one byte past `len` is the terminator.
        assert_eq!(unsafe { *c.ptr.add(c.len) }, 0);
        // Slices share bytes; C gets them as is when a NUL follows, else
        // a pinned copy.
        let whole = rok_str_concat(s(b"ab"), s(b"cd"));
        let tail = rok_str_slice(whole, 2, 4, s(b""));
        assert_eq!(tail.ptr, whole.ptr.wrapping_add(2));
        assert_eq!(rok_str_cstr(tail), tail.ptr);
        let head = rok_str_slice(whole, 0, 2, s(b""));
        assert_eq!(head.ptr, whole.ptr);
        let c = rok_str_cstr(head);
        assert_ne!(c, head.ptr);
        // SAFETY: a NUL-terminated copy of two bytes.
        assert_eq!(unsafe { core::slice::from_raw_parts(c, 3) }, b"ab\0");
        // SAFETY: `c` came from `rok_str_cstr(head)`.
        unsafe { rok_str_cstr_done(head, c) };
        let empty = rok_str_slice(whole, 4, 4, s(b""));
        assert_eq!((empty.ptr, empty.len), (whole.ptr.wrapping_add(4), 0));
        assert_eq!(rok_str_bytes_buf(head).wrapping_add(HEADER_SIZE), head.ptr);
    }
}
