//! f-string format spec:
//! `[[fill]align][sign]['#']['0'][width]['.' precision][type]`.
//!
//! The one spec parser: the runtime parses every spec with it at format
//! time and the compiler (`rokc_hir`, through `#[path]`) checks static
//! ones with it. Uses `core` only, so both crates can build it.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FmtSpec {
    pub fill: char,
    /// `0` (none) or `<` `>` `^` `=`.
    pub align: u8,
    /// `0` (none) or `+` `-` ` `.
    pub sign: u8,
    pub alt: bool,
    pub zero: bool,
    pub width: Option<usize>,
    pub precision: Option<usize>,
    /// `0` (none) or one of `d b o x X f e E g s ?`.
    pub ty: u8,
}

const fn is_align(c: char) -> bool {
    matches!(c, '<' | '>' | '^' | '=')
}

/// The parsed spec, or what is wrong with it.
pub fn parse(text: &str) -> Result<FmtSpec, &'static str> {
    let mut spec = FmtSpec {
        fill: ' ',
        align: 0,
        sign: 0,
        alt: false,
        zero: false,
        width: None,
        precision: None,
        ty: 0,
    };
    let mut rest = text;
    let mut it = rest.chars();
    match (it.next(), it.next()) {
        (Some(f), Some(a)) if is_align(a) => {
            spec.fill = f;
            spec.align = a as u8;
            rest = &rest[f.len_utf8() + 1..];
        }
        (Some(a), _) if is_align(a) => {
            spec.align = a as u8;
            rest = &rest[1..];
        }
        _ => {}
    }
    let mut b = rest.as_bytes();
    if let [s @ (b'+' | b'-' | b' '), tail @ ..] = b {
        spec.sign = *s;
        b = tail;
    }
    if let [b'#', tail @ ..] = b {
        spec.alt = true;
        b = tail;
    }
    if let [b'0', tail @ ..] = b {
        spec.zero = true;
        b = tail;
    }
    spec.width = number(&mut b)?;
    if let [b'.', tail @ ..] = b {
        b = tail;
        spec.precision = Some(number(&mut b)?.ok_or("precision expected after `.`")?);
    }
    if let [t, tail @ ..] = b {
        if !matches!(
            t,
            b'd' | b'b' | b'o' | b'x' | b'X' | b'f' | b'e' | b'E' | b'g' | b's' | b'?'
        ) {
            return Err("unknown format type (expected one of `d b o x X f e E g s ?`)");
        }
        spec.ty = *t;
        b = tail;
    }
    if b.is_empty() {
        Ok(spec)
    } else {
        Err("malformed spec (`[[fill]align][sign][#][0][width][.precision][type]`)")
    }
}

fn number(b: &mut &[u8]) -> Result<Option<usize>, &'static str> {
    let n = b.iter().take_while(|c| c.is_ascii_digit()).count();
    if n == 0 {
        return Ok(None);
    }
    let mut v: usize = 0;
    for &c in &b[..n] {
        v = v
            .checked_mul(10)
            .and_then(|v| v.checked_add(usize::from(c - b'0')))
            .ok_or("width / precision too large")?;
    }
    *b = &b[n..];
    Ok(Some(v))
}

/// An absent width / precision in [`pack`]'s counts word.
const NO_COUNT: u64 = 0xFFFF_FFFF;

/// The no-spec words.
#[allow(dead_code)] // the compiler's
pub const PLAIN: (u64, u64) = (b' ' as u64, NO_COUNT | (NO_COUNT << 32));

/// `#` in [`pack`]'s flags.
#[allow(dead_code)] // the compiler's
pub const ALT: u64 = 1 << 56;

/// `spec` as the two words `core::fmt::Formatter` carries: flags - fill
/// (bits 0..32), align, sign, type (a byte each from bit 32; `s d` as
/// none), `#` (bit 56), `0` (bit 57) - and counts - width, precision
/// (32 bits each, all ones when absent).
#[must_use]
pub fn pack(s: &FmtSpec) -> (u64, u64) {
    let ty = if matches!(s.ty, b's' | b'd') { 0 } else { s.ty };
    let flags = u64::from(u32::from(s.fill))
        | (u64::from(s.align) << 32)
        | (u64::from(s.sign) << 40)
        | (u64::from(ty) << 48)
        | (u64::from(s.alt) << 56)
        | (u64::from(s.zero) << 57);
    let count = |v: Option<usize>| {
        v.map_or(NO_COUNT, |n| {
            u64::try_from(n).unwrap_or(NO_COUNT).min(NO_COUNT - 1)
        })
    };
    (flags, count(s.width) | (count(s.precision) << 32))
}

/// [`pack`]'s inverse.
#[must_use]
#[allow(clippy::cast_possible_truncation)] // fields of the packed words
pub fn unpack(flags: u64, counts: u64) -> FmtSpec {
    let byte = |at: u32| (flags >> at) as u8;
    let count = |v: u64| (v & NO_COUNT != NO_COUNT).then_some((v & NO_COUNT) as usize);
    FmtSpec {
        fill: char::from_u32(flags as u32).unwrap_or(' '),
        align: byte(32),
        sign: byte(40),
        alt: (flags >> 56) & 1 != 0,
        zero: (flags >> 57) & 1 != 0,
        width: count(counts),
        precision: count(counts >> 32),
        ty: byte(48),
    }
}
