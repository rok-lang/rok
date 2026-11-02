//! Names of block-declared items: `outer/name`,
//! `outer` the enclosing fn's lifted path (`f`, `f/g`, `Type.m`), repeats
//! numbered (`f/S/2`). No source name contains `/`.

use std::borrow::Cow;

/// The name as written: `f/S` → `S`, `f/S/2` → `S`, a method of one
/// `f/S/2.m` → `S.m`. Other names unchanged.
#[must_use]
pub fn leaf(name: &str) -> Cow<'_, str> {
    let Some((head, last)) = name.rsplit_once('/') else {
        return Cow::Borrowed(name);
    };
    let (counter, method) = match last.split_once('.') {
        Some((c, m)) => (c, Some(m)),
        None => (last, None),
    };
    if counter.is_empty() || !counter.bytes().all(|b| b.is_ascii_digit()) {
        return Cow::Borrowed(last);
    }
    let base = head.rsplit('/').next().unwrap_or(head);
    match method {
        Some(m) => Cow::Owned(format!("{base}.{m}")),
        None => Cow::Borrowed(base),
    }
}

/// The lifted path of the fn it is declared in: `f/g/S/2` → `f/g`.
#[must_use]
pub fn outer(name: &str) -> &str {
    let mut rest = name;
    if let Some((head, last)) = rest.rsplit_once('/') {
        if last.bytes().all(|b| b.is_ascii_digit()) {
            rest = head;
        }
    }
    rest.rsplit_once('/').map_or("", |(o, _)| o)
}

/// The name with the fns it is declared in: `f/g/S/2` → `f::g::S`,
/// `Type.m/S` → `Type::m::S`.
#[must_use]
pub fn qualified(name: &str) -> String {
    name.split('/')
        .filter(|p| !p.is_empty() && !p.bytes().all(|b| b.is_ascii_digit()))
        .flat_map(|p| p.split('.'))
        .collect::<Vec<_>>()
        .join("::")
}

/// `text` (a type or function path, no file paths) with every lifted
/// name shown as written (`leaf`).
#[must_use]
pub fn unmangle(text: &str) -> Cow<'_, str> {
    if !text.contains('/') {
        return Cow::Borrowed(text);
    }
    let word = |c: char| c.is_alphanumeric() || matches!(c, '_' | '.' | '/');
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(|c| word(c) || c == '\'' || c == '"') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let end = match rest.chars().next() {
            // A `char` / `str` const argument (`Sep<'/'>`) as written.
            Some(q @ ('\'' | '"')) => {
                let mut escaped = false;
                let close = rest[1..].find(|c| {
                    let done = c == q && !escaped;
                    escaped = c == '\\' && !escaped;
                    done
                });
                close.map_or(rest.len(), |i| i + 2)
            }
            _ => rest.find(|c| !word(c)).unwrap_or(rest.len()),
        };
        let token = &rest[..end];
        let lifted = token.starts_with(|c: char| c.is_alphabetic() || c == '_')
            && !token.split('/').any(str::is_empty);
        if lifted {
            out.push_str(&leaf(token));
        } else {
            out.push_str(token);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn local_names_show_as_written() {
        assert_eq!(super::leaf("f/S"), "S");
        assert_eq!(super::leaf("f/g/S/2"), "S");
        assert_eq!(super::leaf("f/S/2.m"), "S.m");
        assert_eq!(super::leaf("Type.m/S"), "S");
        assert_eq!(super::leaf("S"), "S");
        assert_eq!(super::qualified("f/g/S/2"), "f::g::S");
        assert_eq!(super::outer("f/g/S/2"), "f/g");
        assert_eq!(super::outer("Type.m/S"), "Type.m");
        assert_eq!(super::qualified("Type.m/S"), "Type::m::S");
        assert_eq!(
            super::unmangle("m::f/S<i32, f/T/2> | none"),
            "m::S<i32, T> | none"
        );
        assert_eq!(super::unmangle("List<i32>"), "List<i32>");
        assert_eq!(super::unmangle("Sep<'/'> f/S"), "Sep<'/'> S");
        assert_eq!(super::unmangle(r#"T<"a/b\"/c"> f/S"#), r#"T<"a/b\"/c"> S"#);
    }
}
