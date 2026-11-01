//! f-string rendering (refs-lang 1.7.3, stdlib 2.10): numbers under a
//! spec, padding, escapes.
//!
//! The one implementation: the runtime's `rok_fmt_*` write GC text with
//! it, and the compiler (`rokc_hir`, through `#[path]`) evaluates
//! f-strings in constants with it (5.2.5: a constant equals the run-time
//! result). Uses `core` only, so both crates can build it.

use core::fmt::Write;

use crate::fmt_spec::{FmtSpec, pack, parse};

/// Text being written.
pub trait Text: Write {
    fn len(&self) -> usize;
    fn put(&mut self, b: &[u8]);
    /// Every byte written.
    fn bytes_mut(&mut self) -> &mut [u8];
    fn truncate(&mut self, len: usize);

    /// The text written from `start`.
    fn tail_mut(&mut self, start: usize) -> &mut [u8] {
        let b = self.bytes_mut();
        let start = start.min(b.len());
        &mut b[start..]
    }

    /// `count` copies of `c` at byte `at`, moving what follows.
    fn insert(&mut self, at: usize, c: char, count: usize) {
        let mut tmp = [0; 4];
        let one = c.encode_utf8(&mut tmp).as_bytes();
        for _ in 0..count {
            self.put(one);
        }
        self.tail_mut(at).rotate_right(one.len() * count);
    }
}

/// What a malformed spec panics with.
pub fn bad_spec(w: &mut impl Write, spec: &str, why: &str) {
    let _ = write!(w, "invalid format spec `{spec}`: {why}");
}

/// `rok_fmt_spec_parse`'s words: fill, align, sign, `#`, `0`, width,
/// precision (-1 when absent), type (0 when absent or the Display
/// letters `s d`).
pub fn spec_words(spec: &str) -> Result<[i64; 8], &'static str> {
    let s = parse(spec)?;
    let opt = |v: Option<usize>| v.map_or(-1, |v| v as i64);
    let ty = if matches!(s.ty, b's' | b'd') { 0 } else { s.ty };
    Ok([
        i64::from(u32::from(s.fill)),
        s.align.into(),
        s.sign.into(),
        s.alt.into(),
        s.zero.into(),
        opt(s.width),
        opt(s.precision),
        ty.into(),
    ])
}

/// `rok_fmt_pack`'s words; `text_only` refuses a number's type letter.
pub fn pack_spec(spec: &str, text_only: bool) -> Result<(u64, u64), &'static str> {
    let parsed = parse(spec)?;
    if text_only && !matches!(parsed.ty, 0 | b's' | b'd' | b'?') {
        return Err("the type letter needs a number");
    }
    Ok(pack(&parsed))
}

/// The text of `v` (an `f32` widened when `is_f32`): Display for `ty`
/// 0, else `e`, `E`, `f`, `g` or `?` (Debug: Display, and `.0` on a
/// whole number without a precision). Without a precision the digits are
/// the shortest that read back as the same value of its own width; `f`
/// then takes 6 places. `alt` keeps `g`'s trailing zeros. Every float
/// rendering comes here.
#[allow(clippy::cast_possible_truncation)] // `f32` values widened by the caller
pub fn float(out: &mut impl Text, v: f64, is_f32: bool, ty: u8, prec: Option<usize>, alt: bool) {
    let start = out.len();
    let x = v as f32;
    if ty == b'g' && v.is_finite() {
        let p = prec.unwrap_or(6).clamp(1, MAX_SIG);
        let _ = if is_f32 {
            write!(out, "{x:.*e}", p - 1)
        } else {
            write!(out, "{v:.*e}", p - 1)
        };
        general(out, start, p, alt);
        return;
    }
    let exp = ty == b'e' || ty == b'E';
    let _ = match (exp, ty == b'f', prec, is_f32) {
        (true, _, Some(d), true) => write!(out, "{x:.d$e}"),
        (true, _, Some(d), false) => write!(out, "{v:.d$e}"),
        (true, _, None, true) => write!(out, "{x:e}"),
        (true, _, None, false) => write!(out, "{v:e}"),
        (_, true, d, _) => write!(out, "{v:.*}", d.unwrap_or(6)),
        (_, _, Some(d), _) => write!(out, "{v:.d$}"),
        (_, _, None, true) => write!(out, "{x}"),
        (_, _, None, false) => write!(out, "{v}"),
    };
    if ty == b'?' && prec.is_none() && v.is_finite() && !out.tail_mut(start).contains(&b'.') {
        out.put(b".0");
    }
    if ty == b'E' {
        // `inf` / `NaN` hold no `e`.
        out.tail_mut(start)
            .iter_mut()
            .filter(|c| **c == b'e')
            .for_each(|c| *c = b'E');
    }
}

/// The most significant digits `g` shows.
const MAX_SIG: usize = 800;

/// `g`, the general format of C and Python, from the `d.ddde<x>` text
/// written since `start` (`p` significant digits, already rounded):
/// fixed when `-4 <= x < p`, else `d.ddde+XX` (two exponent digits at
/// least). Trailing zeros, and a `.` left bare, go unless `alt`.
fn general(out: &mut impl Text, start: usize, p: usize, alt: bool) {
    let mut sig = [0u8; MAX_SIG];
    let mut n = 0;
    let mut exp: i32 = 0;
    let mut neg = false;
    let mut in_exp = false;
    for &c in &*out.tail_mut(start) {
        match c {
            b'e' | b'E' => in_exp = true,
            b'-' if in_exp => neg = true,
            b'0'..=b'9' if in_exp => exp = exp * 10 + i32::from(c - b'0'),
            b'0'..=b'9' if n < MAX_SIG => {
                sig[n] = c;
                n += 1;
            }
            _ => {}
        }
    }
    if neg {
        exp = -exp;
    }
    out.truncate(start);
    let fixed = (-4..i32::try_from(p).unwrap_or(i32::MAX)).contains(&exp);
    // The digits before the `.`, and the `0`s after it before `frac`.
    let (k, lead) = match (fixed, usize::try_from(exp)) {
        (true, Ok(e)) => ((e + 1).min(n), 0),
        (true, Err(_)) => (0, exp.unsigned_abs() as usize - 1),
        (false, _) => (1.min(n), 0),
    };
    if k == 0 {
        out.put(b"0");
    } else {
        out.put(&sig[..k]);
    }
    let frac = &sig[k..n];
    let keep = if alt {
        frac.len()
    } else {
        frac.iter().rposition(|&c| c != b'0').map_or(0, |i| i + 1)
    };
    if keep > 0 || alt {
        out.put(b".");
        for _ in 0..lead {
            out.put(b"0");
        }
        out.put(&frac[..keep]);
    }
    if !fixed {
        let sign = if exp < 0 { '-' } else { '+' };
        let _ = write!(out, "e{sign}{:02}", exp.unsigned_abs());
    }
}

/// The decimal digits `d` of an integer in `1.5e3` form: shortest
/// exact without a precision, else rounded half up to `prec` fraction
/// digits.
pub fn exp_digits(out: &mut impl Text, d: &[u8], prec: Option<usize>, upper: bool) {
    if d.is_empty() {
        out.put(b"0e0");
        return;
    }
    let mut exp = d.len() - 1;
    // `k` significant digits: `d[..keep]`, then `bump` on the last one
    // kept (a carry turns trailing `9`s into `0`s), then `0`s.
    let k = match prec {
        None => d.iter().rposition(|&c| c != b'0').map_or(1, |i| i + 1),
        Some(p) => p.saturating_add(1),
    };
    let mut keep = k.min(d.len());
    let mut bump = false;
    if d.len() > k && d[k] >= b'5' {
        if let Some(i) = d[..k].iter().rposition(|&c| c != b'9') {
            keep = i;
            bump = true;
        } else {
            // 9.99 rounds to 10.0: one more exponent.
            keep = 0;
            exp += 1;
        }
    }
    let lead = match (bump, keep) {
        (true, _) => Some(d[keep] + 1),
        (false, 0) => Some(b'1'),
        _ => None,
    };
    let sig = d[..keep].iter().copied().chain(lead);
    let zeros = k - keep - usize::from(lead.is_some());
    for (i, c) in sig.chain(core::iter::repeat_n(b'0', zeros)).enumerate() {
        out.put(&[c]);
        if i == 0 && k > 1 {
            out.put(b".");
        }
    }
    let _ = write!(out, "{}{exp}", if upper { 'E' } else { 'e' });
}

/// What the sign shows: `-`, or what `+` / ` ` ask for.
const fn sign(spec: &FmtSpec, neg: bool) -> &'static [u8] {
    match (neg, spec.sign) {
        (true, _) => b"-",
        (false, b'+') => b"+",
        (false, b' ') => b" ",
        _ => b"",
    }
}

const fn radix(ty: u8) -> u32 {
    match ty {
        b'b' => 2,
        b'o' => 8,
        b'x' | b'X' => 16,
        _ => 10,
    }
}

/// Cut what was written since `start` to `precision` chars.
fn cut(w: &mut impl Text, spec: &FmtSpec, start: usize) {
    if let Some(p) = spec.precision {
        let t = w.tail_mut(start);
        let end = core::str::from_utf8(t)
            .ok()
            .and_then(|s| s.char_indices().nth(p))
            .map_or(t.len(), |(i, _)| i);
        w.truncate(start + end);
    }
}

/// Pad what was written since `start` to `width` (2.10.8). `numeric`
/// aligns right by default and lets `0` / `=` fill at `mid`, after the
/// sign and prefix.
fn pad(w: &mut impl Text, spec: &FmtSpec, start: usize, mid: usize, numeric: bool) {
    let Some(width) = spec.width else {
        return;
    };
    let n = w
        .tail_mut(start)
        .iter()
        .filter(|&&b| (b & 0xC0) != 0x80)
        .count();
    if width <= n {
        return;
    }
    let gap = width - n;
    let (mut fill, mut align) = (spec.fill, spec.align);
    if numeric && spec.zero && align == 0 {
        fill = '0';
        align = b'=';
    }
    let (at, before) = match align {
        b'=' if numeric => (mid, gap),
        b'<' => (start, 0),
        0 if !numeric => (start, 0),
        b'^' => (start, gap / 2),
        _ => (start, gap),
    };
    let end = w.len();
    w.insert(end, fill, gap - before);
    w.insert(at, fill, before);
}

/// How a number's body is written.
#[derive(Clone, Copy, Default)]
struct Body {
    /// Pads like text (inf / NaN).
    text: bool,
    /// Uppercase hex digits (`X`).
    upper: bool,
    /// `.` and this many `0`s after (`f` on an integer).
    zeros: usize,
}

/// A number: its sign, `prefix` under `#`, `body`, padded.
fn number<T: Text>(
    w: &mut T,
    spec: &FmtSpec,
    neg: bool,
    prefix: &[u8],
    body: impl FnOnce(&mut T),
    how: Body,
) {
    let start = w.len();
    w.put(sign(spec, neg));
    if spec.alt {
        w.put(prefix);
    }
    let mid = w.len();
    body(w);
    if how.upper {
        w.tail_mut(mid).make_ascii_uppercase();
    }
    if how.zeros > 0 {
        w.put(b".");
        for _ in 0..how.zeros {
            w.put(b"0");
        }
    }
    pad(w, spec, start, mid, !how.text);
}

const fn radix_prefix(ty: u8) -> &'static [u8] {
    match ty {
        b'b' => b"0b",
        b'o' => b"0o",
        _ => b"0x",
    }
}

/// `rok_fmt_digits`: an integer from its sign and its digits in the
/// spec's radix (decimal for `e` / `E` / `f`).
pub fn digits(w: &mut impl Text, spec: &FmtSpec, neg: bool, d: &[u8]) {
    let plain = Body::default();
    match spec.ty {
        b'X' => number(
            w,
            spec,
            neg,
            b"0x",
            |w| w.put(d),
            Body {
                upper: true,
                ..plain
            },
        ),
        b'b' | b'o' | b'x' => number(w, spec, neg, radix_prefix(spec.ty), |w| w.put(d), plain),
        b'e' | b'E' => {
            let upper = spec.ty == b'E';
            number(
                w,
                spec,
                neg,
                b"",
                |w| exp_digits(w, d, spec.precision, upper),
                plain,
            );
        }
        b'g' => {
            let p = spec.precision.unwrap_or(6).clamp(1, MAX_SIG);
            let body = |w: &mut _| {
                let at = Text::len(w);
                exp_digits(w, d, Some(p - 1), false);
                general(w, at, p, spec.alt);
            };
            number(w, spec, neg, b"", body, plain);
        }
        b'f' => {
            let zeros = spec.precision.unwrap_or(6);
            number(w, spec, neg, b"", |w| w.put(d), Body { zeros, ..plain });
        }
        _ => number(w, spec, neg, b"", |w| w.put(d), plain),
    }
}

/// `rok_fmt_int`: the bits `value` of a `bits`-wide integer, `signed` or
/// not. `b o x X` show a signed one's two's complement at that width, or
/// its sign and magnitude when `bits` is 0 (`int`).
pub fn int(w: &mut impl Text, spec: &FmtSpec, value: i64, signed: bool, bits: u32) {
    let base = u64::from(radix(spec.ty));
    let (neg, mag) = if signed && (base == 10 || bits == 0) {
        (value < 0, value.unsigned_abs())
    } else if bits == 0 || bits >= 64 {
        (false, value as u64)
    } else {
        (false, (value as u64) & ((1u64 << bits) - 1))
    };
    let mut tmp = [0u8; 64];
    let mut at = tmp.len();
    let mut rest = mag;
    loop {
        at -= 1;
        tmp[at] = b"0123456789abcdef"[(rest % base) as usize];
        rest /= base;
        if rest == 0 {
            break;
        }
    }
    digits(w, spec, neg, &tmp[at..]);
}

/// `rok_fmt_integral` (`Formatter::pad_integral`): the sign, `prefix`
/// under `#`, `buf`.
pub fn integral(w: &mut impl Text, spec: &FmtSpec, nonneg: bool, prefix: &[u8], buf: &[u8]) {
    number(w, spec, !nonneg, prefix, |w| w.put(buf), Body::default());
}

/// `rok_fmt_f64`'s `mode` bits: the value is an `f32` widened; Debug.
pub const FLOAT_F32: u32 = 1;
pub const FLOAT_DEBUG: u32 = 2;

/// `rok_fmt_f64`: a float (`mode`: `FLOAT_*` bits); inf / NaN pad like
/// text. A binary / octal / hex spec is the panic message.
pub fn float_number(
    w: &mut impl Text,
    spec: &FmtSpec,
    v: f64,
    mode: u32,
) -> Result<(), &'static str> {
    if radix(spec.ty) != 10 {
        return Err("invalid format spec: a float has no binary / octal / hex form");
    }
    let neg = v.is_sign_negative() && !v.is_nan();
    let how = Body {
        text: !v.is_finite(),
        ..Body::default()
    };
    let ty = if mode & FLOAT_DEBUG != 0 {
        b'?'
    } else {
        spec.ty
    };
    let is_f32 = mode & FLOAT_F32 != 0;
    let body = |w: &mut _| float(w, v.abs(), is_f32, ty, spec.precision, spec.alt);
    number(w, spec, neg, b"", body, how);
    Ok(())
}

/// `rok_fmt_pad`: pad what was written since `start` per the spec: as
/// text (cut to `precision` chars first) unless `numeric`, else as a
/// number whose digits begin at `mid`.
pub fn pad_span(w: &mut impl Text, spec: &FmtSpec, start: usize, mid: usize, numeric: bool) {
    if !numeric {
        cut(w, spec, start);
    }
    pad(w, spec, start, mid, numeric);
}

/// `src` as a rok literal between `quote`s (`"` for `str`, `'` for
/// `char`), escaped so the lexer reads it back (`Debug`). Control chars
/// and the bidi controls the lexer rejects (refs-lang 1.1) go out as
/// `\u{..}`.
pub fn escape(w: &mut impl Text, src: &str, quote: u32) {
    let delim = [quote as u8];
    w.put(&delim);
    for ch in src.chars() {
        match ch {
            '\\' => w.put(b"\\\\"),
            '\n' => w.put(b"\\n"),
            '\r' => w.put(b"\\r"),
            '\t' => w.put(b"\\t"),
            '\0' => w.put(b"\\0"),
            ch if ch as u32 == quote => w.put(&[b'\\', delim[0]]),
            ch if ch.is_control()
                || matches!(ch, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}') =>
            {
                let _ = write!(w, "\\u{{{:x}}}", ch as u32);
            }
            ch => w.put(ch.encode_utf8(&mut [0; 4]).as_bytes()),
        }
    }
    w.put(&delim);
}
