//! `#[deprecated]` (spec 6.2.7), `#[deprecated_safe]` (6.2.14),
//! `#[internal]` (6.2.16) and `#[discardable]` (6.2.11): what an item's
//! attributes say about using it.

use rokc_ast::{AttrArg, AttrBody, Attribute, Literal};
use rokc_core::codec_struct;

/// The marks on one item.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mark {
    /// `#[deprecated]`.
    pub deprecated: Option<Deprecation>,
    /// `#[deprecated_safe("note")]`: its note (empty: none).
    pub deprecated_safe: Option<String>,
    /// `#[internal("what to write instead")]`: only the toolchain's
    /// packages may use it; its note (empty: none).
    pub internal: Option<String>,
    /// `#[discardable]`: a call statement may drop the result.
    pub discardable: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Deprecation {
    /// What to use instead (empty: none given).
    pub note: String,
    /// `since = ".."`: the release that deprecated it.
    pub since: Option<String>,
}

codec_struct!(Mark {
    deprecated,
    deprecated_safe,
    internal,
    discardable
});
codec_struct!(Deprecation { note, since });

/// How a `deprecated` warning's note names the release.
const SINCE: &str = "deprecated since ";

impl Deprecation {
    /// The notes of a warning about a use of `name`: since when, and what
    /// to write instead.
    #[must_use]
    pub fn notes(&self, name: &str) -> Vec<String> {
        let mut out: Vec<String> = self.since.iter().map(|s| format!("{SINCE}{s}")).collect();
        out.push(if self.note.is_empty() {
            format!(
                "help: its definition names no replacement; to keep using it, put \
                 `#[allow(deprecated)]` on the item that uses `{name}`"
            )
        } else {
            self.note.clone()
        });
        out
    }
}

fn text(l: &Literal) -> Option<String> {
    match l {
        Literal::Str(s) | Literal::RawStr(s) => Some(s.trim_matches('"').to_owned()),
        _ => None,
    }
}

/// `#[deprecated]`, `#[deprecated = "note"]`, `#[deprecated("note")]` or
/// `#[deprecated(since = "0.1.6", note = "..")]`; `None` when malformed.
#[must_use]
pub fn deprecation(a: &Attribute) -> Option<Deprecation> {
    let mut d = Deprecation::default();
    let args = match &a.body {
        AttrBody::Empty => return Some(d),
        AttrBody::Value(l) => {
            d.note = text(l)?;
            return Some(d);
        }
        AttrBody::Args(args) => args,
    };
    if let [AttrArg::Lit(l)] = args.as_slice() {
        d.note = text(l)?;
        return Some(d);
    }
    let (mut note, mut since) = (None, None);
    for x in args {
        let (k, v) = match x {
            AttrArg::KeyValue(k, v) => (k.as_str(), v),
            AttrArg::Nested { path, body } => match (path.as_slice(), &**body) {
                ([k], AttrBody::Value(v)) => (k.as_str(), v),
                _ => return None,
            },
            _ => return None,
        };
        let slot = match k {
            "note" => &mut note,
            "since" => &mut since,
            _ => return None,
        };
        if slot.replace(text(v)?).is_some() {
            return None;
        }
    }
    if since.as_deref().is_some_and(|s| !is_release(s)) {
        return None;
    }
    d.note = note.unwrap_or_default();
    d.since = since;
    Some(d)
}

/// `0.1.6`: dot-separated numbers.
fn is_release(s: &str) -> bool {
    !s.is_empty()
        && s.split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// `#[x("note")]` / `#[x = "note"]`: the note (empty: none).
fn note(a: &Attribute) -> String {
    match &a.body {
        AttrBody::Value(l) => text(l).unwrap_or_default(),
        AttrBody::Args(xs) => match xs.as_slice() {
            [AttrArg::Lit(l)] => text(l).unwrap_or_default(),
            _ => String::new(),
        },
        AttrBody::Empty => String::new(),
    }
}

/// The marks among `attrs`, if any.
#[must_use]
pub fn mark_of(attrs: &[Attribute]) -> Option<Mark> {
    let mut m = Mark::default();
    for a in attrs {
        match a.path.as_slice() {
            [n] if n == "deprecated" => m.deprecated = deprecation(a),
            [n] if n == "internal" => m.internal = Some(note(a)),
            [n] if n == "discardable" => m.discardable = true,
            [n] if n == "deprecated_safe" => m.deprecated_safe = Some(note(a)),
            _ => {}
        }
    }
    (m != Mark::default()).then_some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attr(src: &str) -> Attribute {
        let src = format!("{src}\nfn f() {{}}\n");
        let (tokens, _) = rokc_lexer::Lexer::new(&src, "test").lex_all();
        let (p, _) = rokc_parser::parse_program(&tokens, "test");
        let rokc_ast::Item::Fn(f) = &p.items[0] else {
            panic!("a fn")
        };
        f.attrs[0].clone()
    }

    #[test]
    fn deprecation_forms() {
        let d = |s: &str| deprecation(&attr(s));
        assert_eq!(d("#[deprecated]").unwrap().note, "");
        assert_eq!(d("#[deprecated = \"use g\"]").unwrap().note, "use g");
        assert_eq!(d("#[deprecated(\"use g\")]").unwrap().note, "use g");
        let full = d("#[deprecated(since = \"0.1.6\", note = \"use g\")]").unwrap();
        assert_eq!(
            (full.note.as_str(), full.since.as_deref()),
            ("use g", Some("0.1.6"))
        );
        assert!(d("#[deprecated(since = \"soon\")]").is_none());
        assert!(d("#[deprecated(note = \"a\", note = \"b\")]").is_none());
        assert!(d("#[deprecated(reason = \"a\")]").is_none());
        assert!(d("#[deprecated(1)]").is_none());
    }
}
