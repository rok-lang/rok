//! f-string support (refs-lang 1.7.3, stdlib 2.10): the GC text a
//! `Formatter` writes, over the renderings in `fmt_text`. Per-type
//! dispatch lives in `core::fmt`.

// `GcBuf` elements start 8-aligned after the header.
#![allow(clippy::cast_ptr_alignment)]

use core::ptr;

use rokr_common::object_header::{HEADER_SIZE, ObjectHeader};

use crate::fmt_spec::unpack;
use crate::fmt_text::{self, Text};
use crate::str::{RokStr, alloc_str, text};

/// A panic message (cut at its size).
struct Buf {
    b: [u8; 256],
    n: usize,
}

impl Buf {
    const fn new() -> Self {
        Self { b: [0; 256], n: 0 }
    }
}

impl core::fmt::Write for Buf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let end = self.n + s.len();
        if end > self.b.len() {
            return Err(core::fmt::Error);
        }
        self.b[self.n..end].copy_from_slice(s.as_bytes());
        self.n = end;
        Ok(())
    }
}

/// Panic at `at` with `msg` rendered (cut at the buffer's size).
pub fn panic_with(msg: &dyn core::fmt::Display, at: Option<&[u8]>) -> ! {
    let mut m = Buf::new();
    let _ = core::fmt::write(&mut m, format_args!("{msg}"));
    crate::panic_at(&m.b[..m.n], at)
}

fn fail(spec: &str, why: &str) -> ! {
    let mut m = Buf::new();
    fmt_text::bad_spec(&mut m, spec, why);
    // SAFETY: valid buffer.
    unsafe { crate::rok_panic(m.b.as_ptr(), m.n) }
}

/// `spec` parsed into `out`, a `GcBuf<i64>` of 8 words
/// (`fmt_text::spec_words`). Panics on a malformed spec; only a dynamic
/// one gets here unchecked.
///
/// # Safety
/// `out` is a live `GcBuf<i64>` of at least 8 elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_fmt_spec_parse(spec: RokStr, out: *mut u8) {
    let t = text(&spec);
    let words = fmt_text::spec_words(t).unwrap_or_else(|why| fail(t, why));
    // SAFETY: caller contract; elements start after the header.
    unsafe {
        ptr::copy_nonoverlapping(
            words.as_ptr(),
            out.add(HEADER_SIZE).cast::<i64>(),
            words.len(),
        );
    }
}

/// What is wrong with `spec`, as `rok_fmt_spec_parse` panics with it;
/// empty when it parses.
#[unsafe(no_mangle)]
pub extern "C" fn rok_fmt_spec_error(spec: RokStr) -> RokStr {
    let t = text(&spec);
    let mut w = W::of(rok_fmt_new(0));
    if let Err(why) = fmt_text::spec_words(t) {
        fmt_text::bad_spec(&mut w, t, why);
    }
    rok_fmt_finish(w.done())
}

/// The text of `v` (an `f32` widened when `is_f32 != 0`): Display for
/// `ty` 0, else `e`, `E`, `f`, `g` or `?`, `prec < 0` for none
/// (`fmt_text::float`).
#[unsafe(no_mangle)]
pub extern "C" fn rok_fmt_float(v: f64, is_f32: u32, ty: u32, prec: i64) -> RokStr {
    let mut w = W::of(rok_fmt_new(32));
    let ty = u8::try_from(ty).unwrap_or(0);
    fmt_text::float(
        &mut w,
        v,
        is_f32 != 0,
        ty,
        usize::try_from(prec).ok(),
        false,
    );
    rok_fmt_finish(w.done())
}

// A `core::fmt::Formatter`'s text: a GC string being filled, held as a
// `str`-shaped handle `(text, len)`; its room is what the object's size
// (`gc_word`) leaves past the header and the NUL. Each writing entry
// takes the handle and returns it, longer and moved when the text
// outgrew its object. A spec comes as `fmt_spec::pack`'s two words.
// `rok_fmt_finish` hands the text over in place.

/// A formatter being written.
struct W {
    p: *mut u8,
    len: usize,
    cap: usize,
}

impl W {
    fn of(h: RokStr) -> Self {
        let p = h.ptr.cast_mut();
        // SAFETY: `p` is the payload of a live GC string (`alloc_str`),
        // which the caller roots, so it does not move.
        let size = unsafe { (*p.sub(HEADER_SIZE).cast::<ObjectHeader>()).gc_word };
        Self {
            p,
            len: h.len,
            #[allow(clippy::cast_possible_truncation)] // an object's size
            cap: size as usize - HEADER_SIZE - 1,
        }
    }
    const fn done(self) -> RokStr {
        RokStr {
            ptr: self.p,
            len: self.len,
        }
    }
    /// Room for `n` more bytes (doubling).
    fn reserve(&mut self, n: usize) {
        if self.len + n > self.cap {
            self.grow(n);
        }
    }

    #[cold]
    #[inline(never)]
    fn grow(&mut self, n: usize) {
        let cap = self.cap.saturating_mul(2).max(self.len + n);
        let old = self.p.wrapping_sub(HEADER_SIZE);
        // May collect. The pin keeps the old text alive and in place: the
        // caller's frame roots only the handle's object, not one an earlier
        // growth in this call allocated (nor `rok_fmt_float`'s own).
        // SAFETY: a live GC string (no allocation since it was made, or
        // the handle's); the pin is ours to undo; a fresh object never
        // overlaps the old text's `len` bytes.
        unsafe {
            rokr_gc::pin(old);
            let p = alloc_str(cap);
            ptr::copy_nonoverlapping(self.p, p, self.len);
            rokr_gc::unpin(old);
            self.p = p;
        }
        self.cap = cap;
    }
}

impl Text for W {
    fn len(&self) -> usize {
        self.len
    }
    fn put(&mut self, b: &[u8]) {
        if b.is_empty() {
            return;
        }
        self.reserve(b.len());
        // SAFETY: `reserve` made room; `b` is not in this buffer.
        unsafe { ptr::copy_nonoverlapping(b.as_ptr(), self.p.add(self.len), b.len()) };
        self.len += b.len();
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: `len <= cap` bytes of text.
        unsafe { core::slice::from_raw_parts_mut(self.p, self.len) }
    }
    fn truncate(&mut self, len: usize) {
        self.len = self.len.min(len);
    }
}

impl core::fmt::Write for W {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.put(s.as_bytes());
        Ok(())
    }
}

/// An empty formatter with room for `cap` bytes.
#[unsafe(no_mangle)]
pub extern "C" fn rok_fmt_new(cap: usize) -> RokStr {
    RokStr {
        ptr: alloc_str(cap),
        len: 0,
    }
}

/// The text, as a `str` in place: NUL-terminated (the allocation has a
/// byte past `cap`). The handle is spent.
#[unsafe(no_mangle)]
pub const extern "C" fn rok_fmt_finish(h: RokStr) -> RokStr {
    // SAFETY: the allocation holds a byte past the room `len` is within.
    unsafe { h.ptr.cast_mut().add(h.len).write(0) };
    h
}

#[unsafe(no_mangle)]
pub const extern "C" fn rok_fmt_len(h: RokStr) -> usize {
    h.len
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_fmt_str(h: RokStr, s: RokStr) -> RokStr {
    let mut w = W::of(h);
    w.put(s.bytes());
    w.done()
}

#[unsafe(no_mangle)]
pub extern "C" fn rok_fmt_char(h: RokStr, c: u32) -> RokStr {
    let c = char::from_u32(c).unwrap_or(char::REPLACEMENT_CHARACTER);
    let mut w = W::of(h);
    w.put(c.encode_utf8(&mut [0; 4]).as_bytes());
    w.done()
}

/// `s` as a rok literal between `quote`s, escaped.
#[unsafe(no_mangle)]
pub extern "C" fn rok_fmt_escaped(h: RokStr, s: RokStr, quote: u32) -> RokStr {
    let mut w = W::of(h);
    fmt_text::escape(&mut w, text(&s), quote);
    w.done()
}

/// An integer under the spec `flags` / `counts` (`fmt_text::int`).
#[unsafe(no_mangle)]
pub extern "C" fn rok_fmt_int(
    h: RokStr,
    flags: u64,
    counts: u64,
    value: i64,
    signed: u32,
    bits: u32,
) -> RokStr {
    let mut w = W::of(h);
    fmt_text::int(&mut w, &unpack(flags, counts), value, signed != 0, bits);
    w.done()
}

/// An integer from its sign and `d`, its digits in the spec's radix
/// (decimal for `e` / `E` / `f`).
#[unsafe(no_mangle)]
pub extern "C" fn rok_fmt_digits(
    h: RokStr,
    flags: u64,
    counts: u64,
    neg: u32,
    d: RokStr,
) -> RokStr {
    let mut w = W::of(h);
    fmt_text::digits(&mut w, &unpack(flags, counts), neg != 0, d.bytes());
    w.done()
}

/// `Formatter::pad_integral`: the sign, `prefix` under `#`, `buf`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_fmt_integral(
    h: RokStr,
    flags: u64,
    counts: u64,
    nonneg: u32,
    prefix: RokStr,
    buf: RokStr,
) -> RokStr {
    let mut w = W::of(h);
    let spec = unpack(flags, counts);
    fmt_text::integral(&mut w, &spec, nonneg != 0, prefix.bytes(), buf.bytes());
    w.done()
}

/// A float (`mode`: 1 an `f32` widened, 2 Debug); inf / NaN pad like
/// text.
#[unsafe(no_mangle)]
pub extern "C" fn rok_fmt_f64(h: RokStr, flags: u64, counts: u64, v: f64, mode: u32) -> RokStr {
    let mut w = W::of(h);
    if let Err(m) = fmt_text::float_number(&mut w, &unpack(flags, counts), v, mode) {
        // SAFETY: valid message.
        unsafe { crate::rok_panic(m.as_ptr(), m.len()) }
    }
    w.done()
}

/// Pad what was written since `start` per the spec: as text (cut to
/// `precision` chars first) when `numeric` is 0, else as a number whose
/// digits begin at `mid`.
#[unsafe(no_mangle)]
pub extern "C" fn rok_fmt_pad(
    h: RokStr,
    flags: u64,
    counts: u64,
    start: usize,
    mid: usize,
    numeric: u32,
) -> RokStr {
    let mut w = W::of(h);
    fmt_text::pad_span(&mut w, &unpack(flags, counts), start, mid, numeric != 0);
    w.done()
}

/// The two spec words (`fmt_spec::pack`) of `spec`, parsed at run time
/// (a dynamic spec) into `out`; panics when malformed, or when
/// `text_only` and it has a number's type letter.
///
/// # Safety
/// `out` is a live `GcBuf<u64>` of at least 2 elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rok_fmt_pack(spec: RokStr, text_only: u32, out: *mut u8) {
    let src = text(&spec);
    let (flags, counts) =
        fmt_text::pack_spec(src, text_only != 0).unwrap_or_else(|why| fail(src, why));
    // SAFETY: caller contract; elements start after the header.
    unsafe {
        let words = out.add(HEADER_SIZE).cast::<u64>();
        words.write(flags);
        words.add(1).write(counts);
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::fmt_spec::{FmtSpec, pack, parse};

    const fn s(b: &'static str) -> RokStr {
        RokStr {
            ptr: b.as_ptr(),
            len: b.len(),
        }
    }

    // Allocates: called from the GC test (single mutator).
    pub fn fmt_ops() {
        let f = |v: f64, ty: u8, prec: i64| text(&rok_fmt_float(v, 0, ty.into(), prec)).to_owned();
        assert_eq!(f(0.1 + 0.2, 0, -1), "0.30000000000000004");
        assert_eq!(f(1.0, 0, -1), "1");
        assert_eq!(f(1e21, 0, -1), "1000000000000000000000");
        assert_eq!(f(f64::NAN, 0, -1), "NaN");
        assert_eq!(f(-f64::INFINITY, b'E', -1), "-inf");
        assert_eq!(f(core::f64::consts::PI, 0, 2), "3.14");
        assert_eq!(f(12345.678, b'e', -1), "1.2345678e4");
        assert_eq!(f(12345.678, b'e', 3), "1.235e4");
        assert_eq!(f(12345.678, b'E', 2), "1.23E4");
        assert_eq!(f(1.5, b'f', -1), "1.500000");
        let f32_text = |v: f32, ty: u8| text(&rok_fmt_float(v.into(), 1, ty.into(), -1)).to_owned();
        assert_eq!(f32_text(0.1, 0), "0.1");
        assert_eq!(f32_text(0.1, b'e'), "1e-1");
        let e = |d: &'static str, p: i64| {
            let mut w = W::of(rok_fmt_new(1));
            fmt_text::exp_digits(&mut w, d.as_bytes(), usize::try_from(p).ok(), false);
            text(&rok_fmt_finish(w.done())).to_owned()
        };
        assert_eq!(e("42", -1), "4.2e1");
        assert_eq!(e("1000", -1), "1e3");
        assert_eq!(e("12345", 2), "1.23e4");
        assert_eq!(e("99999", 1), "1.0e5");
        assert_eq!(
            parse("x>"),
            Ok(FmtSpec {
                fill: 'x',
                align: b'>',
                sign: 0,
                alt: false,
                zero: false,
                width: None,
                precision: None,
                ty: 0
            })
        );
        assert!(parse(".").is_err());
        assert!(parse("5q").is_err());
        assert!(parse("1.2.3").is_err());
        let sp = parse("*^+#012.3x").unwrap();
        let (fl, co) = pack(&sp);
        assert_eq!(unpack(fl, co), sp);
    }

    // Allocates: called from the GC test (single mutator).
    pub fn general_and_debug() {
        let f = |v: f64, ty: u8, prec: i64| text(&rok_fmt_float(v, 0, ty.into(), prec)).to_owned();
        assert_eq!(f(1234.5678, b'g', 3), "1.23e+03");
        assert_eq!(f(1e20, b'g', -1), "1e+20");
        assert_eq!(f(1234.5678, b'g', -1), "1234.57");
        assert_eq!(f(0.0001, b'g', -1), "0.0001");
        assert_eq!(f(0.000_012_34, b'g', -1), "1.234e-05");
        assert_eq!(f(100.0, b'g', 0), "1e+02");
        assert_eq!(f(0.0, b'g', -1), "0");
        assert_eq!(f(999_999.5, b'g', -1), "1e+06");
        assert_eq!(f(f64::NAN, b'g', -1), "NaN");
        assert_eq!(f(1.0, b'?', -1), "1.0");
        assert_eq!(f(2.5, b'?', -1), "2.5");
        assert_eq!(f(f64::INFINITY, b'?', -1), "inf");
        assert_eq!(f(1.0, b'?', 2), "1.00");
        let fmt = |spec: &str, write: &dyn Fn(RokStr, u64, u64) -> RokStr| {
            let (fl, co) = pack(&parse(spec).unwrap());
            let t = rok_fmt_str(rok_fmt_new(1), s("<"));
            text(&rok_fmt_finish(write(t, fl, co))).to_owned()
        };
        let float =
            |v: f64, mode: u32| move |t: RokStr, fl: u64, co: u64| rok_fmt_f64(t, fl, co, v, mode);
        let int = |v: i64| move |t: RokStr, fl: u64, co: u64| rok_fmt_int(t, fl, co, v, 1, 32);
        assert_eq!(fmt("#.3g", &float(1.0, 0)), "<1.00");
        assert_eq!(fmt("+10.3g", &float(-1234.5678, 0)), "< -1.23e+03");
        assert_eq!(fmt("g", &int(1_234_567)), "<1.23457e+06");
        assert_eq!(fmt("g", &int(-42)), "<-42");
        assert_eq!(fmt(">6?", &float(1.0, 0)), "<   1.0");
        assert_eq!(fmt("", &float(3.0, 2)), "<3.0");
        assert_eq!(fmt("", &float(-0.5, 2)), "<-0.5");
    }

    // Allocates: called from the GC test (single mutator).
    pub fn exp_rounding() {
        let e = |d: &'static str, p: i64| {
            let mut w = W::of(rok_fmt_new(1));
            fmt_text::exp_digits(&mut w, d.as_bytes(), usize::try_from(p).ok(), false);
            text(&rok_fmt_finish(w.done())).to_owned()
        };
        assert_eq!(e("19", 0), "2e1");
        assert_eq!(e("1295", 2), "1.30e3");
        assert_eq!(e("5", 3), "5.000e0");
        // No precision cap.
        assert_eq!(e("12", 600).len(), 2 + 600 + 2);
        assert_eq!(text(&rok_fmt_float(0.1, 0, 0, 700)).len(), 2 + 700);
    }

    // Allocates: called from the GC test (single mutator).
    pub fn formatter_ops() {
        // The formatter: `spec` applied to what `write` adds to "<".
        let fmt = |spec: &str, write: &dyn Fn(RokStr, u64, u64) -> RokStr| {
            let (fl, co) = pack(&parse(spec).unwrap());
            let t = rok_fmt_str(rok_fmt_new(1), s("<"));
            text(&rok_fmt_finish(write(t, fl, co))).to_owned()
        };
        let int = |v: i64, signed: u32, bits: u32| {
            move |t: RokStr, fl: u64, co: u64| rok_fmt_int(t, fl, co, v, signed, bits)
        };
        assert_eq!(fmt("", &int(-120, 1, 64)), "<-120");
        assert_eq!(fmt("", &int(i64::MIN, 1, 64)), "<-9223372036854775808");
        assert_eq!(fmt("", &int(-1, 0, 64)), "<18446744073709551615");
        assert_eq!(fmt("x", &int(-1, 1, 8)), "<ff");
        assert_eq!(fmt("x", &int(-255, 1, 0)), "<-ff");
        assert_eq!(fmt("#X", &int(0xCAFE, 1, 32)), "<0xCAFE");
        assert_eq!(fmt("08b", &int(42, 1, 32)), "<00101010");
        assert_eq!(fmt("=+5", &int(42, 1, 32)), "<+  42");
        assert_eq!(fmt("*^7", &int(42, 1, 32)), "<**42***");
        assert_eq!(fmt("06", &int(-42, 1, 32)), "<-00042");
        assert_eq!(fmt("e", &int(42, 1, 32)), "<4.2e1");
        assert_eq!(fmt(".2f", &int(3, 1, 32)), "<3.00");
        let float = |v: f64| move |t: RokStr, fl: u64, co: u64| rok_fmt_f64(t, fl, co, v, 0);
        assert_eq!(fmt("10.4", &float(core::f64::consts::PI)), "<    3.1416");
        assert_eq!(fmt("08", &float(f64::INFINITY)), "<inf     ");
        let pad = |t: RokStr, fl: u64, co: u64| {
            let start = rok_fmt_len(t);
            let t = rok_fmt_str(t, s("héllo world"));
            rok_fmt_pad(t, fl, co, start, start, 0)
        };
        assert_eq!(fmt("", &pad), "<héllo world");
        assert_eq!(fmt("_>8.4", &pad), "<____héll");
        let esc = |t: RokStr, _: u64, _: u64| {
            rok_fmt_escaped(t, s("a\"'\n\u{1}\u{85}\u{202E}é"), u32::from('"'))
        };
        assert_eq!(fmt("", &esc), "<\"a\\\"'\\n\\u{1}\\u{85}\\u{202e}é\"");
        // Grows past its first allocation; the text stays NUL-terminated.
        let long = |t: RokStr, _: u64, _: u64| (0..40).fold(t, |t, _| rok_fmt_str(t, s("ab")));
        assert_eq!(fmt("", &long).len(), 81);
        let done = rok_fmt_finish(rok_fmt_str(rok_fmt_new(1), s("xyz")));
        // SAFETY: one byte past `len` is the terminator.
        assert_eq!(unsafe { *done.ptr.add(done.len) }, 0);
    }
}
