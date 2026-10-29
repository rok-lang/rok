use crate::edit::{self, Step};
use crate::{Doc, Kind, Value, parse, to_string};

fn kind(src: &str) -> Kind {
    parse(src).unwrap_or_else(|e| panic!("{src:?}: {e:?}")).kind
}

#[test]
#[allow(clippy::cognitive_complexity)]
fn every_value_kind() {
    assert!(matches!(kind("none"), Kind::None));
    assert!(matches!(kind("true"), Kind::Bool(true)));
    assert!(matches!(kind("false"), Kind::Bool(false)));
    for (src, want) in [
        ("42", 42),
        ("-7", -7),
        ("0xff", 255),
        ("0b1010", 10),
        ("0o17", 15),
        ("1_000_000", 1_000_000),
        ("-0x10", -16),
        ("18446744073709551615", 18_446_744_073_709_551_615),
    ] {
        assert!(matches!(kind(src), Kind::Int(i) if i == want), "{src}");
    }
    for (src, want) in [
        ("2.5", 2.5_f64),
        ("-1.5e-3", -1.5e-3),
        ("2E10", 2e10),
        ("1_0.5", 10.5),
    ] {
        assert!(
            matches!(kind(src), Kind::Float(f) if f.to_bits() == want.to_bits()),
            "{src}"
        );
    }
    assert!(matches!(kind("'a'"), Kind::Char('a')));
    assert!(matches!(kind("'\\n'"), Kind::Char('\n')));
    assert!(matches!(kind("\"a\\tb\""), Kind::Str(s) if s == "a\tb"));
    assert!(matches!(kind("r\"a\\b\""), Kind::Str(s) if s == "a\\b"));
    assert!(matches!(kind("\"two\nlines\""), Kind::Str(s) if s == "two\nlines"));
    assert!(matches!(kind("[1, 2,]"), Kind::List(v) if v.len() == 2));
    assert!(matches!(kind("[]"), Kind::List(v) if v.is_empty()));
    assert!(matches!(kind("(1, \"a\")"), Kind::Tuple(v) if v.len() == 2));
    assert!(matches!(kind("{}"), Kind::Dict(v) if v.is_empty()));
    assert!(matches!(kind("{ \"a\": 1, 2: [3], }"), Kind::Dict(v) if v.len() == 2));
    assert!(
        matches!(kind("{ x: 1, y: 2 }"), Kind::Struct { name: None, fields } if fields.len() == 2)
    );
    assert!(
        matches!(kind("Point { x: 1, y: -2 }"), Kind::Struct { name: Some(n), .. } if n == "Point")
    );
    assert!(matches!(kind("Point {}"), Kind::Struct { fields, .. } if fields.is_empty()));
    assert!(
        matches!(kind("Mode::Fast"), Kind::Variant { path, args, .. } if path == "Mode::Fast" && args.is_empty())
    );
    assert!(matches!(
        kind("Shape::Circle(1.0, at: (0, 0))"),
        Kind::Variant { args, named, braces: false, .. } if args.len() == 1 && named[0].name == "at"
    ));
    assert!(matches!(
        kind("Shape::Rect { w: 2, h: 3 }"),
        Kind::Variant { named, braces: true, .. } if named.len() == 2
    ));
    assert!(matches!(
        kind("// lead\n/* block /* nested */ */ [1, // one\n 2]"),
        Kind::List(v) if v.len() == 2
    ));
}

/// Every rejected form: its message and its `line:col`.
#[test]
fn every_error() {
    for (src, want, at) in [
        ("", "expected a value, found the end of the file", "1:1"),
        ("// only a comment\n", "expected a value", "2:1"),
        ("1 2", "a ROKON file holds one value", "1:3"),
        ("{ a: 1 } { b: 2 }", "a ROKON file holds one value", "1:10"),
        ("[x]", "bare identifier `x` is not a ROKON value", "1:2"),
        ("[1 + 2]", "operator `+` is not allowed", "1:4"),
        ("[!true]", "operator `!` is not allowed", "1:2"),
        ("-x", "`-` goes only before a number", "1:1"),
        ("[a.b()]", "method calls are not allowed", "1:2"),
        ("f(1)", "calls are not allowed", "1:1"),
        ("P { ..q }", "`..` is not allowed", "1:5"),
        ("1..2", "ranges (`..`) are not allowed", "1:1"),
        ("{ let x = 1; x }", "blocks are not allowed", "1:1"),
        ("[f\"{1}\"]", "f-strings are not allowed", "1:2"),
        ("{ 1, 2 }", "sets are not ROKON values", "1:1"),
        ("{ a: 1, \"b\": 2 }", "either fields", "1:9"),
        ("{ \"b\": 2, a: 1 }", "either fields", "1:11"),
        ("P {\n    x,\n}", "field `x` needs a value", "2:5"),
        ("42i32", "type suffixes are not allowed", "1:1"),
        ("1.5f64", "type suffixes are not allowed", "1:1"),
        ("[1e999]", "float `1e999` is out of range", "1:2"),
        ("-1e999", "float `1e999` is out of range", "1:2"),
        ("c'a'", "`c'..'` literals are not allowed", "1:1"),
        ("List::<i32>::new", "generic arguments", "1:1"),
        ("\"open", "unterminated string", "1:1"),
        ("[1,\n 2", "", "2:3"),
    ] {
        let e = parse(src).expect_err(src);
        let r = e.render(src, "f.rokon");
        assert!(r.contains(want), "{src:?}: {r}");
        assert!(
            r.starts_with("error[E002]: ") && r.contains(&format!("--> f.rokon:{at}\n")),
            "{src:?}: {r}"
        );
    }
    // The caret is under the offending token.
    let src = "Config {\n    name: bare,\n}";
    assert_eq!(
        parse(src).unwrap_err().render(src, "c.rokon"),
        "error[E002]: bare identifier `bare` is not a ROKON value (a string is quoted: \
         `\"bare\"`; an enum variant is written with its type: `Type::bare`)\n  --> c.rokon:2:11\n   |\n 2 |     name: bare,\n   |           ^^^^"
    );
}

#[test]
fn canonical_text_round_trips() {
    for src in [
        "none",
        "[1, -2, 3.5, -0.25, 1.0e-7, 'q', '\\'', \"s\\\"\\n\", true]",
        "[1.0e17, -1.5e300, 0.00001, 12345.5, 0.0, 10000000000000000.0, 1.0e-6]",
        "\"\\u{202e}x\\u{2066}\"",
        "(1,)",
        "()",
        "{}",
        "{ \"a\": 1, 2: none, (1, 2): [] }",
        "Point { x: 1, y: 2 }",
        "{ x: 1 }",
        "Mode::Fast",
        "Shape::Circle(1.5, at: (0, 0))",
        "Shape::Rect { w: 2, h: 3 }",
        "P {}",
    ] {
        let v = parse(src).unwrap();
        let text = to_string(&v);
        assert_eq!(text, src, "canonical form of {src:?}");
        assert_eq!(parse(&text).unwrap(), v);
    }
    // A variant of named values only has one form, `E::V { .. }`.
    assert_eq!(to_string(&parse("E::V(k: 1)").unwrap()), "E::V { k: 1 }");
    // Too wide for a line: one entry per line, trailing commas, nested
    // values on one line when they fit.
    let long = "x".repeat(50);
    let v = parse(&format!(
        "Lock{{version:1,packages:[Locked{{name:\"a\",source:Dep::git(\"{long}\",tag:\"v1\"),\
         commit:\"{long}\"}}],empty:[]}}"
    ))
    .unwrap();
    let text = to_string(&v);
    assert_eq!(
        text,
        format!(
            "Lock {{\n    version: 1,\n    packages: [\n        Locked {{\n            name: \
             \"a\",\n            source: Dep::git(\"{long}\", tag: \"v1\"),\n            \
             commit: \"{long}\",\n        }},\n    ],\n    empty: [],\n}}"
        )
    );
    assert_eq!(parse(&text).unwrap(), v);
    // A value exactly at the width stays on one line; one more breaks it.
    let fits = |n: usize| to_string(&Value::str("a".repeat(n))).lines().count();
    assert_eq!(fits(200), 1, "a scalar never breaks");
    let list = |n: usize| to_string(&parse(&format!("[\"{}\"]", "a".repeat(n))).unwrap());
    assert_eq!(list(96).lines().count(), 1);
    assert_eq!(list(97), format!("[\n    \"{}\",\n]", "a".repeat(97)));
}

#[test]
fn typed_decoding() {
    type Check = fn(crate::Decoder<'_>) -> Result<(), String>;
    let text = "Cfg {\n    name: \"n\",\n    n: 300,\n    ratio: 2,\n    tags: [\"a\"],\n    \
                pair: (1, 'c'),\n    map: { \"k\": true },\n    proxy: none,\n    \
                dep: Dep::git(\"u\", tag: \"t\"),\n}";
    let doc = Doc::parse(text, "c.rokon").unwrap();
    let r = doc.root().record("Cfg").unwrap();
    r.only(&["name", "n", "ratio", "tags", "pair", "map", "proxy", "dep"])
        .unwrap();
    assert_eq!(r.req("name").unwrap().string().unwrap(), "n");
    assert_eq!(r.req("n").unwrap().int::<u16>().unwrap(), 300);
    let e = r.req("n").unwrap().int::<u8>().unwrap_err();
    assert!(
        e.starts_with("error[E200]: 300 is out of range for u8\n  --> c.rokon:3:8"),
        "{e}"
    );
    assert!((r.req("ratio").unwrap().float().unwrap() - 2.0).abs() < f64::EPSILON);
    assert_eq!(
        r.req("tags").unwrap().list().unwrap()[0].string().unwrap(),
        "a"
    );
    let pair = r.req("pair").unwrap().tuple(2).unwrap();
    assert_eq!(pair[1].char().unwrap(), 'c');
    let map = r.req("map").unwrap().dict().unwrap();
    assert_eq!(map[0].0.string().unwrap(), "k");
    assert!(map[0].1.bool().unwrap());
    // `T | none`: missing and `none` read as `None`.
    assert!(r.opt("proxy").is_none() && r.opt("absent").is_none());
    assert!(r.get("proxy").is_some());
    let e = r.req("absent").unwrap_err();
    assert!(
        e.starts_with("error[E200]: missing field `absent`\n  --> c.rokon:1:1"),
        "{e}"
    );
    let d = r.req("dep").unwrap().variant("Dep").unwrap();
    assert_eq!(d.name, "git");
    assert_eq!(d.args[0].string().unwrap(), "u");
    assert_eq!(d.fields.req("tag").unwrap().string().unwrap(), "t");
    let e = d.fields.only(&["rev"]).unwrap_err();
    assert!(e.contains("unknown argument `tag` (expected rev)"), "{e}");

    // The target type decides: `{}` and `{ field: .. }` are structs,
    // `{}` and `{ "k": .. }` dicts.
    let check = |src: &str, f: &dyn Fn(crate::Decoder<'_>) -> Result<(), String>| {
        let doc = Doc::parse(src, "t.rokon").unwrap();
        f(doc.root())
    };
    check("{}", &|d| d.record("X").map(drop)).unwrap();
    check("{}", &|d| d.dict().map(drop)).unwrap();
    check("{ a: 1 }", &|d| d.record("X").map(drop)).unwrap();
    let cases: &[(&str, Check, &str)] = &[
        (
            "Y { }",
            |d| d.record("X").map(drop),
            "expected `X { .. }`, found a struct `Y { .. }`",
        ),
        ("{ a: 1 }", |d| d.dict().map(drop), "write `\"a\": ..`"),
        (
            "{ \"a\": 1, \"a\": 2 }",
            |d| d.dict().map(drop),
            "key \"a\" is given twice",
        ),
        (
            "X { a: 1, a: 2 }",
            |d| d.record("X").map(drop),
            "field `a` is given twice",
        ),
        (
            "X { b: 1 }",
            |d| d.record("X")?.only(&["a"]),
            "unknown field `b` (expected a)",
        ),
        (
            "Other::V",
            |d| d.variant("E").map(drop),
            "expected a `E::..` variant, found `Other::V`",
        ),
        (
            "[1]",
            |d| d.string().map(drop),
            "expected a string, found a list",
        ),
        (
            "{ a: [] }",
            |d| d.record("X")?.req("a")?.string().map(drop),
            "`a` must be a string",
        ),
        ("(1, 2)", |d| d.tuple(3).map(drop), "expected a tuple of 3"),
        (
            "1.5",
            |d| d.int::<i64>().map(drop),
            "expected an integer, found a float",
        ),
    ];
    for &(src, f, want) in cases {
        let e = check(src, &f).unwrap_err();
        assert!(e.contains(want), "{src:?}: {e}");
        assert!(e.contains("--> t.rokon:1:"), "{e}");
    }
}

const APP: &str = "// the app\nPackage {\n    name: \"app\",\n    deps: {\n        \"a\": \
                   Dep::path(\"../a\"), // local\n        \"b\": Dep::git(\"u\", tag: \"v1\"),\n    \
                   },\n    dev_deps: { \"c\": Dep::git(\"u2\") },\n}\n";

#[test]
fn edits_keep_comments_and_layout() {
    let deps = |k| [Step::Field("deps"), Step::Key(k)];
    let x = "Dep::path(\"x\")";
    // Replace in place, keeping the trailing comment.
    let t = edit::set(APP, &deps("a"), x).unwrap();
    assert_eq!(t, APP.replace("Dep::path(\"../a\")", x));
    // Add to a multi-line dict and to a one-line one.
    let t = edit::set(APP, &deps("d"), x).unwrap();
    assert!(
        t.contains("tag: \"v1\"),\n        \"d\": Dep::path(\"x\"),\n    },"),
        "{t}"
    );
    assert!(t.starts_with("// the app\n"));
    let t = edit::set(APP, &[Step::Field("dev_deps"), Step::Key("d")], x).unwrap();
    assert!(
        t.contains("dev_deps: { \"c\": Dep::git(\"u2\"), \"d\": Dep::path(\"x\") },"),
        "{t}"
    );
    // A missing field is added, then filled (no trailing comma before).
    let t = edit::set("Package {\n    name: \"n\"\n}\n", &deps("d"), x).unwrap();
    assert_eq!(
        t,
        "Package {\n    name: \"n\",\n    deps: {\n        \"d\": Dep::path(\"x\"),\n    },\n}\n"
    );
    // An unnamed root and an empty one.
    let t = edit::set("{}", &deps("d"), x).unwrap();
    assert_eq!(
        t,
        "{\n    deps: {\n        \"d\": Dep::path(\"x\"),\n    },\n}"
    );
    // Lists: replace and append by index.
    let t = edit::set("[1, /* two */ 2]", &[Step::Index(1)], "3").unwrap();
    assert_eq!(t, "[1, /* two */ 3]");
    let t = edit::set("[\n    1, // one\n]", &[Step::Index(1)], "2").unwrap();
    assert_eq!(t, "[\n    1, // one\n    2,\n]");
    assert!(edit::set("[1]", &[Step::Index(5)], "2").is_err());
    assert!(edit::set("[1]", &[Step::Field("a")], "2").is_err());

    // Removing takes the entry's line (comment included) or its text.
    let t = edit::remove(APP, &deps("a")).unwrap().unwrap();
    assert!(t.contains("deps: {\n        \"b\":"), "{t}");
    let t = edit::remove(APP, &[Step::Field("dev_deps"), Step::Key("c")])
        .unwrap()
        .unwrap();
    assert!(t.contains("dev_deps: { },"), "{t}");
    assert!(edit::remove(APP, &deps("zz")).unwrap().is_none());
    assert!(
        edit::remove(APP, &[Step::Field("nope"), Step::Key("a")])
            .unwrap()
            .is_none()
    );
    let t = edit::remove("[1, 2, 3]", &[Step::Index(1)])
        .unwrap()
        .unwrap();
    assert_eq!(t, "[1, 3]");
    // A syntax error is reported, not edited around.
    assert!(edit::set("Package { name: x }", &deps("a"), x).is_err());
}

#[test]
fn quote_escapes_bidi() {
    // The lexer rejects a raw bidi control in a string.
    for (s, want) in [
        ("a\u{202A}b", r#""a\u{202a}b""#),
        ("a\u{202E}b", r#""a\u{202e}b""#),
        ("a\u{2066}b", r#""a\u{2066}b""#),
        ("a\u{2069}b", r#""a\u{2069}b""#),
    ] {
        let q = crate::quote(s);
        assert_eq!(q, want);
        assert!(matches!(kind(&q), Kind::Str(back) if back == s));
    }
}

#[test]
fn huge_float_is_an_error_naming_the_range() {
    for src in ["1e999", "-1e999", "[1.5e400]"] {
        let e = parse(src).expect_err(src);
        assert!(
            e.message
                .contains("is out of range (ROKON floats are f64, up to about 1.8e308)"),
            "{src:?}: {}",
            e.message
        );
    }
}
