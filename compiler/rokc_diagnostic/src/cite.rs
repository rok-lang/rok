//! Spec citations in diagnostic text: `(spec 3.12.5)`, `; stdlib 2.13`.
//! Human output drops them; JSON keeps the text and lists them.

/// Documents a message may cite.
const DOCS: &[&str] = &["spec", "stdlib spec", "stdlib", "refs-lang", "bare", "std"];

/// The end of a section number at `i` (`2.8.3a`), if one starts there.
fn number(s: &str, i: usize) -> Option<usize> {
    let rest = &s[i..];
    if !rest.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let len = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.'))
        .unwrap_or(rest.len());
    Some(i + rest[..len].trim_end_matches('.').len())
}

/// The end of `doc N` at `i`.
fn cite(s: &str, i: usize) -> Option<usize> {
    let rest = &s[i..];
    let doc = DOCS
        .iter()
        .find(|d| rest.strip_prefix(**d).is_some_and(|r| r.starts_with(' ')))?;
    number(s, i + doc.len() + 1)
}

/// The end of a list of citations at `i`: `spec 5.9.4, 8.5`,
/// `spec 3.14 / 4.14`, `refs-lang 1.7.3, stdlib 2.10`.
fn cites(s: &str, i: usize) -> Option<usize> {
    let mut end = cite(s, i)?;
    loop {
        let next = [", ", " / ", "; ", " and "]
            .iter()
            .find(|sep| s[end..].starts_with(**sep))
            .and_then(|sep| {
                let at = end + sep.len();
                cite(s, at).or_else(|| number(s, at))
            });
        match next {
            Some(e) => end = e,
            None => return Some(end),
        }
    }
}

/// Each citation list in `text` with the range it takes out.
fn found(text: &str) -> Vec<(usize, usize, usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if !text.is_char_boundary(i) {
            i += 1;
            continue;
        }
        let Some(end) = DOCS
            .iter()
            .any(|d| text[i..].starts_with(d))
            .then(|| cites(text, i))
            .flatten()
        else {
            i += 1;
            continue;
        };
        let before = &text[..i];
        let after = &text[end..];
        let cut = if before.ends_with('(') {
            let open = i - 1;
            let from = if before[..open].ends_with(' ') {
                open - 1
            } else {
                open
            };
            if after.starts_with(')') {
                Some((from, end + 1))
            } else if after.starts_with(" - ") {
                Some((i, end + 3))
            } else if after.starts_with("; ") {
                Some((i, end + 2))
            } else {
                None
            }
        } else if (before.ends_with("; ") || before.ends_with(", "))
            && (after.is_empty() || after.starts_with([')', '\n']))
        {
            Some((i - 2, end))
        } else {
            None
        };
        if let Some((from, to)) = cut {
            out.push((from, to, i, end));
        }
        i = end;
    }
    out
}

/// `text` without its citations.
#[must_use]
pub fn strip(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (from, to, _, _) in found(text) {
        out.push_str(&text[at..from]);
        at = to;
    }
    out.push_str(&text[at..]);
    out
}

/// The citations `strip` takes out of `text`: `spec 3.12.5`.
#[must_use]
pub fn refs(text: &str) -> Vec<String> {
    found(text)
        .into_iter()
        .map(|(_, _, i, end)| text[i..end].to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{refs, strip};

    #[test]
    fn strips_citations() {
        let cases = [
            ("bad (spec 3.12.5)", "bad"),
            ("bad (spec 3.12.5) at line 3", "bad at line 3"),
            ("x (spec 5.9.4, 8.5) y", "x y"),
            ("x (spec 3.14 / 4.14)", "x"),
            ("(it stays; stdlib 2.13.9)", "(it stays)"),
            ("(a rule, spec 2.9.5)", "(a rule)"),
            ("(spec 5.6 - add `pub`)", "(add `pub`)"),
            ("x (refs-lang 1.7.3, stdlib 2.10)", "x"),
            ("x (stdlib spec 0.1.2)", "x"),
            ("ends (spec 2.8.3a).", "ends."),
            ("the spec 3.2 table", "the spec 3.2 table"),
            ("`spec 2`", "`spec 2`"),
            ("x (spec 2.18.1); use y", "x; use y"),
            ("plain", "plain"),
        ];
        for (text, want) in cases {
            assert_eq!(strip(text), want, "{text}");
        }
    }

    #[test]
    fn lists_citations() {
        assert_eq!(
            refs("a (spec 3.1) b (it; stdlib 2.13)"),
            ["spec 3.1", "stdlib 2.13"]
        );
    }
}
