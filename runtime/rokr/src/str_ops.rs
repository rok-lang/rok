//! The computations behind the runtime's pure `str` entries,
//! on plain bytes. The one implementation: `str.rs` wraps them
//! for compiled code and the compiler's constant evaluator (`rokc_hir`,
//! through `#[path]`) calls them directly. Uses `core` only.

/// Why `b[start..end]` is not a `str` slice; `Display` is the panic
/// message.
pub enum SliceError<'a> {
    Range,
    /// Byte `at` falls inside `ch`, which starts at byte `from`.
    Boundary {
        at: usize,
        from: usize,
        ch: &'a [u8],
    },
}

impl core::fmt::Display for SliceError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Range => f.write_str("string slice out of range"),
            Self::Boundary { at, from, ch } => write!(
                f,
                "string slice is not on a char boundary: byte {at} is inside `{}` \
                 (bytes {from}..{})\nstring offsets count bytes; to work by character, \
                 use `chars()` or `char_indices()`",
                core::str::from_utf8(ch).unwrap_or("?"),
                from + ch.len()
            ),
        }
    }
}

/// Whether `b[start..end]` is a `str` slice: in range, on char
/// boundaries.
pub fn slice_check(b: &[u8], start: usize, end: usize) -> Result<(), SliceError<'_>> {
    if start > end || end > b.len() {
        return Err(SliceError::Range);
    }
    let inside = |i: usize| i < b.len() && (b[i] & 0xC0) == 0x80;
    let Some(at) = [start, end].into_iter().find(|&i| inside(i)) else {
        return Ok(());
    };
    let from = (0..at).rev().find(|&i| !inside(i)).unwrap_or(0);
    let len = match b[from] {
        0xF0.. => 4,
        0xE0.. => 3,
        _ => 2,
    };
    let ch = &b[from..b.len().min(from + len)];
    Err(SliceError::Boundary { at, from, ch })
}

/// Byte offset of the first `n` in `h`.
pub fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    if let [c] = n {
        return h.iter().position(|b| b == c);
    }
    if n.is_empty() {
        return Some(0);
    }
    h.windows(n.len()).position(|w| w == n)
}

/// Byte offset of the last `n` in `h`.
pub fn rfind(h: &[u8], n: &[u8]) -> Option<usize> {
    if n.is_empty() {
        return Some(h.len());
    }
    h.windows(n.len()).rposition(|w| w == n)
}

/// Whether `h[at..at + n.len()]` is `n`.
pub fn eq_at(h: &[u8], at: usize, n: &[u8]) -> bool {
    at.checked_add(n.len()).and_then(|end| h.get(at..end)) == Some(n)
}

/// `h` with every non-overlapping `f` replaced by `t`, piece by piece
/// into `put`. An empty `f` matches at every char boundary: "ab" →
/// "-a-b-".
pub fn replace(h: &[u8], f: &[u8], t: &[u8], mut put: impl FnMut(&[u8])) {
    if f.is_empty() {
        put(t);
        for (i, b) in h.iter().enumerate() {
            put(core::slice::from_ref(b));
            if h.get(i + 1).is_none_or(|n| (n & 0xC0) != 0x80) {
                put(t);
            }
        }
        return;
    }
    let (mut i, mut run) = (0, 0);
    while i + f.len() <= h.len() {
        if &h[i..i + f.len()] == f {
            put(&h[run..i]);
            put(t);
            i += f.len();
            run = i;
        } else {
            i += 1;
        }
    }
    put(&h[run..]);
}

/// `s` as an integer of `bits` bits in base `radix` (2..=36), `signed`
/// or not: an optional sign, then digits of either case. The one integer
/// grammar (`FromStr` and `Int::parse_radix`). The value's bits, or the
/// status: 1 empty, 2 invalid digit, 3 out of range.
pub fn parse_int(s: &[u8], radix: u32, signed: bool, bits: u32) -> Result<u64, i32> {
    let (neg, digits) = match s {
        [b'-', rest @ ..] => (true, rest),
        [b'+', rest @ ..] => (false, rest),
        all => (false, all),
    };
    if digits.is_empty() {
        return Err(1);
    }
    if !(2..=36).contains(&radix) || !digits.iter().all(|&c| char::from(c).is_digit(radix)) {
        return Err(2);
    }
    let mag = digits.iter().try_fold(0u64, |m, &c| {
        let d = char::from(c).to_digit(radix).unwrap_or(0);
        m.checked_mul(radix.into())?.checked_add(d.into())
    });
    let bits = bits.clamp(1, 64);
    let limit = match (signed, neg) {
        (true, true) => 1u64 << (bits - 1),
        (true, false) => (1u64 << (bits - 1)) - 1,
        (false, false) => u64::MAX >> (64 - bits),
        (false, true) => 0,
    };
    let Some(mag) = mag.filter(|&m| (signed || !neg) && m <= limit) else {
        return Err(3);
    };
    Ok(if neg { mag.wrapping_neg() } else { mag })
}

/// `t` as an `f64` (`inf`, `NaN` and exponents included), or the status:
/// 2 invalid, 3 out of range.
pub fn parse_float(t: &str) -> Result<f64, i32> {
    let Ok(v) = t.parse::<f64>() else {
        return Err(2);
    };
    // Digits that round past f64's range (`inf` itself has none).
    if v.is_infinite() && t.bytes().any(|b| b.is_ascii_digit()) {
        return Err(3);
    }
    Ok(v)
}

/// Offset of the first byte of `b` that is not UTF-8, or -1.
pub fn utf8_error(b: &[u8]) -> isize {
    core::str::from_utf8(b).map_or_else(|e| e.valid_up_to() as isize, |_| -1)
}
