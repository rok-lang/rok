//! Escape decoding for string / char literal bodies.
//! Literals keep the raw body; the lexer validates and later passes
//! decode with the same functions.

/// Which literal the body belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EscapeMode {
    /// `"..."` - `\xHH` is ASCII only; `\` + newline continues the line.
    Str,
    /// `'.'` - `\xHH` is ASCII only.
    Char,
    /// `c'.'` - `\xHH` may be any byte value.
    CChar,
}

/// Decode `body`. On error returns `(byte offset of the bad escape, msg)`.
pub fn unescape(body: &str, mode: EscapeMode) -> Result<String, (usize, String)> {
    let mut out = String::with_capacity(body.len());
    let mut it = body.char_indices().peekable();
    while let Some((at, c)) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some((_, e)) = it.next() else {
            return Err((at, "unterminated escape sequence".to_owned()));
        };
        let decoded = match e {
            '\\' => '\\',
            '"' => '"',
            '\'' => '\'',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            '0' => '\0',
            'x' => {
                let hex: String = (0..2)
                    .filter_map(|_| it.next_if(|(_, h)| h.is_ascii_hexdigit()).map(|(_, h)| h))
                    .collect();
                let max = if mode == EscapeMode::CChar {
                    0xFF
                } else {
                    0x7F
                };
                match u32::from_str_radix(&hex, 16) {
                    Ok(v) if hex.len() == 2 && v <= max => char::from_u32(v).unwrap_or('\0'),
                    Ok(_) if hex.len() == 2 => {
                        return Err((at, format!("`\\x{hex}` is out of range (max `\\x{max:X}`)")));
                    }
                    _ => return Err((at, "`\\x` needs exactly two hex digits".to_owned())),
                }
            }
            'u' => {
                if it.next_if(|(_, b)| *b == '{').is_none() {
                    return Err((at, "`\\u` must be followed by `{`".to_owned()));
                }
                let mut hex = String::new();
                let mut prev_underscore = true; // no leading `_`
                loop {
                    match it.next() {
                        Some((_, '}')) if !hex.is_empty() && !prev_underscore => break,
                        Some((_, '_')) if !prev_underscore => prev_underscore = true,
                        Some((_, h)) if h.is_ascii_hexdigit() && hex.len() < 6 => {
                            hex.push(h);
                            prev_underscore = false;
                        }
                        _ => {
                            return Err((at, "malformed `\\u{...}` (1-6 hex digits)".to_owned()));
                        }
                    }
                }
                let v = u32::from_str_radix(&hex, 16).unwrap_or(u32::MAX);
                match char::from_u32(v) {
                    Some(ch) => ch,
                    None => {
                        return Err((at, format!("`\\u{{{hex}}}` is not a Unicode scalar value")));
                    }
                }
            }
            '\n' | '\r' if mode == EscapeMode::Str => {
                // Line continuation: drop `\`, the terminator and the
                // next line's leading whitespace.
                while it
                    .next_if(|(_, w)| matches!(w, ' ' | '\t' | '\n' | '\r' | '\x0B' | '\x0C'))
                    .is_some()
                {}
                continue;
            }
            other => {
                return Err((
                    at,
                    format!("unknown escape sequence `\\{}`", other.escape_debug()),
                ));
            }
        };
        out.push(decoded);
    }
    Ok(out)
}

/// `s` as a `"..."` body that `unescape(_, Str)` gives back.
#[must_use]
pub fn escape_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// `bytes` as a `b"..."` body that `unescape(_, CChar)` gives back, a
/// char per byte.
#[must_use]
pub fn escape_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if b.is_ascii_graphic() && b != b'\\' && b != b'"' || b == b' ' {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("\\x{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{EscapeMode::*, escape_bytes, escape_str, unescape};

    #[test]
    fn escapes_round_trip() {
        let s = "a\"b\\c\n\u{AC00}\\\n";
        assert_eq!(unescape(&escape_str(s), Str).unwrap(), s);
        let b: Vec<u8> = (0..=255).collect();
        let back: Vec<u8> = unescape(&escape_bytes(&b), CChar)
            .unwrap()
            .chars()
            .map(|c| u8::try_from(c).unwrap())
            .collect();
        assert_eq!(back, b);
    }

    #[test]
    fn decodes_and_rejects() {
        assert_eq!(
            unescape(r"a\n\t\\\x41\u{1_F600}\u{AC00}", Str).unwrap(),
            "a\n\t\\A\u{1F600}가"
        );
        assert_eq!(unescape("one \\\n     two", Str).unwrap(), "one two");
        assert_eq!(unescape(r"\xFF", CChar).unwrap(), "\u{FF}");
        for bad in [
            r"\q",
            r"\x80",
            r"\x4",
            r"\u{110000}",
            r"\u{D800}",
            r"\u{}",
            r"\u{_1}",
            r"\u{1234567}",
            "\\",
        ] {
            assert!(unescape(bad, Str).is_err(), "{bad}");
        }
        assert!(unescape("\\\n", Char).is_err());
    }
}
