//! Unicode `char` properties and case mapping
//! over the tables `gen_unicode.py` generates from the UCD.
//!
//! The one implementation: the runtime's `rok_char_*` / `rok_str_case` /
//! `rok_str_trim` and the compiler's constant evaluator (`rokc_hir`,
//! through `#[path]`) both call it. Uses `core` only. Text that is not
//! UTF-8 (`str` can carry raw OS bytes) falls back to ASCII-only
//! trimming and case mapping.

use core::cmp::Ordering;

use crate::unicode_tables as t;

fn in_ranges(table: &[(u32, u32)], c: u32) -> bool {
    table
        .binary_search_by(|&(lo, hi)| {
            if hi < c {
                Ordering::Less
            } else if lo > c {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

fn lookup<V: Copy>(table: &[(u32, V)], c: u32) -> Option<V> {
    table
        .binary_search_by_key(&c, |&(k, _)| k)
        .ok()
        .map(|i| table[i].1)
}

/// Property bits of `c`: 1 Alphabetic, 2 Lowercase, 4 Uppercase,
/// 8 `White_Space`, 16 numeric (general category Nd / Nl / No).
pub fn props(c: u32) -> u32 {
    let tables = [
        t::ALPHABETIC,
        t::LOWERCASE,
        t::UPPERCASE,
        t::WHITE_SPACE,
        t::NUMERIC,
    ];
    let mut bits = 0;
    for (i, table) in tables.into_iter().enumerate() {
        if in_ranges(table, c) {
            bits |= 1 << i;
        }
    }
    bits
}

/// Case mapping `mode`: 0 lowercase, 1 uppercase, 2 titlecase.
pub const LOWER: i32 = 0;
pub const UPPER: i32 = 1;
pub const TITLE: i32 = 2;

/// Simple (one-char) case mapping (`mode` as `LOWER` / `UPPER` /
/// `TITLE`); `c` itself when it has none.
pub fn to_case(c: u32, mode: i32) -> u32 {
    match mode {
        LOWER => lookup(t::TO_LOWER, c).unwrap_or(c),
        UPPER => lookup(t::TO_UPPER, c).unwrap_or(c),
        // Titlecase is the uppercase unless the table says otherwise.
        TITLE => lookup(t::TO_TITLE, c).unwrap_or_else(|| to_case(c, UPPER)),
        _ => c,
    }
}

fn is_white_space(c: char) -> bool {
    in_ranges(t::WHITE_SPACE, c as u32)
}

/// Unicode 3.13 `Final_Sigma`: a cased letter before the `Σ` at byte
/// `i` and none after (case-ignorable chars skipped both ways).
fn final_sigma(s: &str, i: usize) -> bool {
    let cased = |c: char| in_ranges(t::CASED, c as u32);
    let ignorable = |c: char| in_ranges(t::CASE_IGNORABLE, c as u32);
    let before = s[..i].chars().rev().find(|&c| !ignorable(c));
    let after = s[i + 'Σ'.len_utf8()..].chars().find(|&c| !ignorable(c));
    before.is_some_and(cased) && !after.is_some_and(cased)
}

/// The full case mapping of `c` (at byte `i` of `s`), char by char.
fn map_full(s: &str, i: usize, c: char, mode: i32, f: &mut impl FnMut(char)) {
    if c.is_ascii() {
        f(if mode == LOWER {
            c.to_ascii_lowercase()
        } else {
            c.to_ascii_uppercase()
        });
        return;
    }
    let full = match mode {
        LOWER => t::FULL_LOWER,
        UPPER => t::FULL_UPPER,
        _ => t::FULL_TITLE,
    };
    if let Some(m) = lookup(full, c as u32) {
        for &x in m.iter().filter(|&&x| x != 0) {
            f(char::from_u32(x).unwrap_or(c));
        }
    } else if mode == LOWER && c == 'Σ' && final_sigma(s, i) {
        f('ς');
    } else {
        f(char::from_u32(to_case(c as u32, mode)).unwrap_or(c));
    }
}

/// `text` mapped char by char into `f` (`mode` as `to_case`): full
/// Unicode mappings (`ß` → `SS`, `Ss` in titlecase) plus `Final_Sigma`;
/// no locale-specific rules and no word boundaries.
pub fn case(text: &str, mode: i32, mut f: impl FnMut(char)) {
    for (i, c) in text.char_indices() {
        map_full(text, i, c, mode, &mut f);
    }
}

/// `b` trimmed of Unicode `White_Space` (ASCII whitespace when not
/// UTF-8): `which` 0 = both ends, 1 = start, 2 = end.
pub fn trim(b: &[u8], which: i32) -> &[u8] {
    let Ok(text) = core::str::from_utf8(b) else {
        let mut b = b;
        while which != 2 && b.first().is_some_and(u8::is_ascii_whitespace) {
            b = &b[1..];
        }
        while which != 1 && b.last().is_some_and(u8::is_ascii_whitespace) {
            b = &b[..b.len() - 1];
        }
        return b;
    };
    let text = match which {
        1 => text.trim_start_matches(is_white_space),
        2 => text.trim_end_matches(is_white_space),
        _ => text.trim_matches(is_white_space),
    };
    text.as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn props_and_case() {
        assert_eq!(props('A' as u32), 1 | 4);
        assert_eq!(props('한' as u32), 1);
        assert_eq!(props('٣' as u32), 16);
        assert_eq!(props(0x3000), 8);
        assert_eq!(to_case('É' as u32, LOWER), 'é' as u32);
        assert_eq!(to_case('ß' as u32, UPPER), 'ß' as u32);
        assert_eq!(to_case('ǆ' as u32, UPPER), 'Ǆ' as u32);
        assert_eq!(to_case('ǆ' as u32, TITLE), 'ǅ' as u32);
        assert_eq!(to_case('a' as u32, TITLE), 'A' as u32);
        let full = |c: &str, mode| {
            let mut out = String::new();
            case(c, mode, |m| out.push(m));
            out
        };
        assert_eq!(full("ß", TITLE), "Ss");
        assert_eq!(full("ﬁ", TITLE), "Fi");
        assert_eq!(full("ǆ", TITLE), "ǅ");
        assert_eq!(full("ß", UPPER), "SS");
        assert!(final_sigma("ΟΔΟΣ", 6));
        assert!(!final_sigma("Σ", 0));
    }
}
