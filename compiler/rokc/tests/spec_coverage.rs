//! Spec sections against the cases' `// spec:` tags. Writes
//! `target/spec-coverage.{md,json}` and prints a per-part summary.
//!
//! The spec is `$ROK_SPEC_DIR`, else the first `spec/en` (then `spec/ko`)
//! above the workspace; without one the report is skipped.
//!
//! A section (three or more numbers) is covered by a tag naming it or a
//! subsection; a chapter tag covers no section.
//!
//! Tag forms: `003.05.1-name`, `std 209-name §2.9.4`, `std-216.5-name`,
//! `refs-lang 004.12 §4.12.9`, ranges `§4.11.3-4.11.5`.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Domain {
    Lang,
    Std,
}

impl Domain {
    const fn name(self) -> &'static str {
        match self {
            Self::Lang => "lang",
            Self::Std => "std",
        }
    }
}

/// `(domain, [3, 11, 4])`.
type Id = (Domain, Vec<u32>);

fn show(id: &Id) -> String {
    let nums: Vec<String> = id.1.iter().map(u32::to_string).collect();
    format!("{} {}", id.0.name(), nums.join("."))
}

struct Chapter {
    id: Id,
    title: String,
    sections: Vec<(Id, String)>,
}

fn spec_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("ROK_SPEC_DIR") {
        return Some(PathBuf::from(d));
    }
    let ws = rokc_core::real_path(common::workspace()).ok()?;
    ["spec/en", "spec/ko"].iter().find_map(|lang| {
        ws.ancestors()
            .map(|a| a.join(lang))
            .find(|d| d.join("refs-lang").is_dir())
    })
}

fn md_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut es: Vec<PathBuf> = rd.filter_map(|e| Some(e.ok()?.path())).collect();
    es.sort();
    for p in es {
        if p.is_dir() {
            md_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "md") {
            out.push(p);
        }
    }
}

/// `"3.11.4. Title"` -> `([3, 11, 4], "Title")`.
fn numbered(text: &str) -> Option<(Vec<u32>, String)> {
    let (num, title) = text.split_once(' ').unwrap_or((text, ""));
    let num = num.strip_suffix('.').unwrap_or(num);
    let nums: Option<Vec<u32>> = num.split('.').map(|p| p.parse().ok()).collect();
    let nums = nums.filter(|n| n.len() >= 2)?;
    Some((nums, title.trim().to_owned()))
}

fn chapters(spec: &Path) -> Vec<Chapter> {
    let mut out = Vec::new();
    for (dom, sub) in [
        (Domain::Lang, "refs-lang"),
        (Domain::Std, "refs-standard-libraries"),
    ] {
        let mut files = Vec::new();
        md_files(&spec.join(sub), &mut files);
        for f in files {
            let text = fs::read_to_string(&f).unwrap_or_default();
            let mut chapter: Option<Chapter> = None;
            let mut fence = false;
            for line in text.lines() {
                if line.starts_with("```") {
                    fence = !fence;
                }
                if fence {
                    continue;
                }
                let hashes = line.bytes().take_while(|&b| b == b'#').count();
                let Some(rest) = line.get(hashes..).and_then(|r| r.strip_prefix(' ')) else {
                    continue;
                };
                let Some((nums, title)) = numbered(rest) else {
                    continue;
                };
                if hashes == 1 && nums.len() == 2 {
                    chapter = Some(Chapter {
                        id: (dom, nums),
                        title,
                        sections: Vec::new(),
                    });
                } else if (2..=4).contains(&hashes) && nums.len() >= 3 {
                    if let Some(c) = chapter.as_mut() {
                        c.sections.push(((dom, nums), title));
                    }
                }
            }
            out.extend(chapter);
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// `"003.05.1-x"` -> lang `[3, 5, 1]`; std `"216.5"` -> `[2, 16, 5]`,
/// `"000.03.5"` -> `[0, 3, 5]`, `"209"` -> `[2, 9]`.
fn padded(tok: &str, dom: Domain) -> Option<Vec<u32>> {
    let head: String = tok
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = head.trim_end_matches('.').split('.');
    let first = parts.next().filter(|p| p.len() == 3)?;
    let first: u32 = first.parse().ok()?;
    let rest: Option<Vec<u32>> = parts.map(|p| p.parse().ok()).collect();
    let rest = rest?;
    let mut nums = match dom {
        Domain::Lang => vec![first],
        Domain::Std if first % 100 == 0 => {
            if rest.is_empty() {
                return None; // `200-core`: a directory
            }
            vec![first / 100]
        }
        Domain::Std => vec![first / 100, first % 100],
    };
    nums.extend(rest);
    Some(nums)
}

/// `"4.11.3-4.11.5"` -> the three sections; `"2.3"` -> `[2, 3]`.
fn dotted(tok: &str) -> Vec<Vec<u32>> {
    let parse = |s: &str| -> Option<Vec<u32>> {
        let s = s.trim_end_matches(|c: char| !c.is_ascii_digit());
        let v: Option<Vec<u32>> = s.split('.').map(|p| p.parse().ok()).collect();
        v.filter(|v| v.len() >= 2)
    };
    let range = tok
        .split_once('-')
        .and_then(|(a, b)| Some((parse(a)?, parse(b)?)));
    if let Some((a, b)) =
        range.filter(|(a, b)| a.len() == b.len() && a[..a.len() - 1] == b[..a.len() - 1])
    {
        let (lo, hi) = (a[a.len() - 1], b[b.len() - 1]);
        return (lo..=hi.min(lo + 64))
            .map(|n| {
                let mut v = a.clone();
                *v.last_mut().unwrap() = n;
                v
            })
            .collect();
    }
    parse(tok.split('-').next().unwrap_or(tok))
        .into_iter()
        .collect()
}

fn tag_ids(tag: &str) -> Vec<Id> {
    let mut dom = Domain::Lang;
    let mut out = Vec::new();
    for tok in tag.split(|c: char| c.is_whitespace() || matches!(c, ',' | '/' | '+' | '(' | ')')) {
        let mut tok = tok;
        if tok == "std" || tok == "stdlib" {
            dom = Domain::Std;
            continue;
        }
        if tok == "refs-lang" {
            dom = Domain::Lang;
            continue;
        }
        if let Some(r) = tok.strip_prefix("std-") {
            dom = Domain::Std;
            tok = r;
        }
        if let Some(r) = tok.strip_prefix('§') {
            out.extend(dotted(r).into_iter().map(|n| (dom, n)));
        } else if let Some(n) = padded(tok, dom) {
            out.push((dom, n));
        } else if tok.starts_with(|c: char| c.is_ascii_digit()) && tok.contains('.') {
            out.extend(dotted(tok).into_iter().map(|n| (dom, n)));
        }
    }
    out
}

/// `// spec:` lines of a case, outside a multi-line `expect-stdout` block.
fn case_tags(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_stdout = false;
    for line in src.lines() {
        let t = line.trim_start();
        if in_stdout {
            if t == "//" || t.starts_with("// ") {
                continue;
            }
            in_stdout = false;
        }
        if t.trim_end() == "// expect-stdout:" {
            in_stdout = true;
        } else if let Some(rest) = t.strip_prefix("// spec:") {
            out.push(rest.trim().to_owned());
        }
    }
    out
}

fn cases() -> Vec<(String, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/cases");
    let mut out = Vec::new();
    for e in fs::read_dir(&dir).unwrap().filter_map(Result::ok) {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let src = if p.is_dir() {
            fs::read_to_string(p.join("src/main.rok")).ok()
        } else if p.extension().is_some_and(|x| x == "rok") {
            fs::read_to_string(&p).ok()
        } else {
            None
        };
        if let Some(src) = src {
            out.push((name, src));
        }
    }
    out.sort();
    out
}

/// Rust test files whose `// spec:` lines tag the next `fn` (tool and
/// CLI behavior no case can show), relative to the workspace.
const TEST_FILES: &[&str] = &[
    "compiler/rokc/tests/cli.rs",
    "compiler/rokc/tests/devtools.rs",
    "compiler/rokc/tests/native.rs",
    "compiler/rokc/tests/tools.rs",
    "compiler/rok/tests/rok.rs",
];

/// `(file::fn, tags)` of the `// spec:` lines in `src`.
fn fn_tags(file: &str, src: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let mut pending = Vec::new();
    for line in src.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("// spec:") {
            pending.push(rest.trim().to_owned());
        } else if let Some(rest) = t
            .strip_prefix("fn ")
            .or_else(|| t.strip_prefix("async fn "))
        {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !pending.is_empty() {
                out.push((format!("{file}::{name}"), std::mem::take(&mut pending)));
            }
        }
    }
    out
}

fn test_fns() -> Vec<(String, Vec<String>)> {
    let ws = common::workspace();
    let mut out = Vec::new();
    for f in TEST_FILES {
        let src = fs::read_to_string(ws.join(f)).unwrap();
        let file = f.rsplit('/').next().unwrap();
        out.extend(fn_tags(file, &src));
    }
    out
}

/// Sections with no rule a test can check, by kind (tag syntax). They
/// are left out of the counts and listed in the report.
const NON_NORMATIVE: &[(&str, &str)] = &[
    (
        "notation: how the spec is written",
        "refs-lang §0.8.1-0.8.7 std §0.5.2 §0.5.3 §0.5.6 §0.5.7",
    ),
    (
        "rationale: motivation, comparison with other languages, usage advice",
        "refs-lang §0.5.5 §1.3.2 §1.8.2 §2.12.2 §2.12.4 §3.17.8 §4.13.10 §6.4.1 §6.6.1 §6.6.6 \
         §6.6.11 §6.6.14 §6.8.10 §7.1.4 §7.7.10 §9.4.1 §9.4.2 §9.5.3 \
         std §0.2.1 §0.2.7 §0.3.1 §0.3.6 §1.1.7 §1.3.1 §1.3.8 §1.5.3 §1.5.4 §1.6.1 §1.6.5 \
         §1.6.8 §1.7.5 §1.7.7 §1.7.8 §1.8.3 §1.8.4 §1.8.5 §2.1.10 §2.2.9 §2.6.1 §2.6.9 \
         §2.9.11 §2.10.13 §2.11.11 §2.12.6 §2.13.7 \
         §2.14.9 §2.15.9 §2.20.10 §2.22.4 §3.3.6 §3.8.8 §3.9.6",
    ),
    (
        "summary: restates rules given elsewhere",
        "refs-lang §2.4.7 §2.14.5 §4.8.10 §5.8.13 §6.2.13 §6.7.14 §6.8.12 §6.9.10 §8.9.14 \
         §8.10.11 §9.6.6 §9.9.15 std §2.2.8 §3.3.8",
    ),
    (
        "pointer: defers to another section, no rule of its own",
        "refs-lang §0.4.1 §0.4.2 §1.5.2 §2.8.8 §2.14.7 §3.1.2 §6.4.10 §6.4.11 §6.5.9 §8.7.1 §9.6.7 \
         §9.7.11 §9.8.9 std §2.7.12 §2.12.1",
    ),
    (
        "policy: versioning and stability promises, not observable in one build",
        "refs-lang §1.1.4 §1.4.5 §6.4.9 §8.1.10 §8.2.11 §8.3.9 §8.5.9 §8.6.14 §9.1.10 §9.3.8 \
         §9.4.11 §9.5.11 std §0.1.5",
    ),
    (
        "informative: compiler internals, costs, out-of-scope lists, non-normative appendices",
        "refs-lang §0.2.1 §0.2.2 §1.2.4 §2.2.9 §3.3.4 §7.2.11 §8.4.5 §8.4.9 §8.5.12 §8.7.2 \
         §6.8.9 §8.3.7 §8.7.8 §8.7.9 §9.2.1 §9.2.7 §9.2.9 §9.5.6 §9.8.8",
    ),
];

fn non_normative() -> BTreeMap<Id, &'static str> {
    let mut out = BTreeMap::new();
    for (kind, ids) in NON_NORMATIVE {
        for id in tag_ids(ids) {
            let shown = show(&id);
            assert!(out.insert(id, *kind).is_none(), "{shown} listed twice");
        }
    }
    out
}

fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn pct(a: usize, b: usize) -> f64 {
    if b == 0 {
        0.0
    } else {
        a as f64 * 100.0 / b as f64
    }
}

#[test]
fn spec_coverage_report() {
    let Some(spec) = spec_dir() else {
        println!("spec-coverage: no spec/en or spec/ko found (set ROK_SPEC_DIR); skipped");
        return;
    };
    let chapters = chapters(&spec);
    assert!(!chapters.is_empty(), "no numbered chapters under {spec:?}");

    let known: BTreeSet<Id> = chapters
        .iter()
        .flat_map(|c| std::iter::once(c.id.clone()).chain(c.sections.iter().map(|s| s.0.clone())))
        .collect();

    // id -> cases naming it.
    let mut tagged: BTreeMap<Id, BTreeSet<String>> = BTreeMap::new();
    let mut unresolved: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut untagged = 0;
    let cases = cases();
    let mut sources: Vec<(String, Vec<String>)> = Vec::new();
    for (name, src) in &cases {
        let tags = case_tags(src);
        if tags.is_empty() {
            untagged += 1;
        }
        sources.push((name.clone(), tags));
    }
    let fns = test_fns();
    sources.extend(fns.iter().cloned());
    for (name, tags) in &sources {
        for tag in tags {
            let ids = tag_ids(tag);
            if !ids.iter().any(|i| known.contains(i)) {
                unresolved
                    .entry(tag.clone())
                    .or_default()
                    .insert(name.clone());
            }
            for id in ids {
                tagged.entry(id).or_default().insert(name.clone());
            }
        }
    }
    let covering = |sec: &Id| -> BTreeSet<String> {
        tagged
            .range(sec.clone()..)
            .take_while(|(k, _)| k.0 == sec.0 && k.1.starts_with(&sec.1))
            .flat_map(|(_, v)| v.iter().cloned())
            .collect()
    };

    let skip = non_normative();
    let stale: Vec<String> = skip
        .keys()
        .filter(|id| !known.contains(*id))
        .map(show)
        .collect();

    let mut md = String::new();
    let mut json = String::from("{\n");
    let (mut all_secs, mut all_cov, mut all_skip) = (0, 0, 0);
    // Part (`lang 3`) -> (normative sections, covered, non-normative).
    let mut parts: BTreeMap<(Domain, u32), (usize, usize, usize)> = BTreeMap::new();
    let mut chapter_rows = String::new();
    let mut section_rows = String::new();
    let mut json_chapters = Vec::new();
    for c in &chapters {
        let (mut cov, mut skipped) = (0, 0);
        let mut json_secs = Vec::new();
        for (sid, title) in &c.sections {
            let by = covering(sid);
            let kind = skip.get(sid).copied();
            if kind.is_some() {
                skipped += 1;
            } else if !by.is_empty() {
                cov += 1;
            }
            let mark = match kind {
                Some(k) => k.split(':').next().unwrap(),
                None if by.is_empty() => "-",
                None => "✓",
            };
            let _ = writeln!(
                section_rows,
                "| {} | {} | {mark} | {} |",
                show(sid),
                title.replace('|', "\\|"),
                by.iter().take(4).cloned().collect::<Vec<_>>().join(", ")
                    + if by.len() > 4 { ", ..." } else { "" }
            );
            json_secs.push(format!(
                "{{\"id\":{},\"title\":{},\"non_normative\":{},\"cases\":[{}]}}",
                json_str(&show(sid)),
                json_str(title),
                kind.is_some(),
                by.iter().map(|s| json_str(s)).collect::<Vec<_>>().join(",")
            ));
        }
        let n = c.sections.len() - skipped;
        let chapter_only = tagged.get(&c.id).map_or(0, BTreeSet::len);
        let p = parts.entry((c.id.0, c.id.1[0])).or_default();
        p.0 += n;
        p.1 += cov;
        p.2 += skipped;
        all_secs += n;
        all_cov += cov;
        all_skip += skipped;
        let _ = writeln!(
            chapter_rows,
            "| {} | {} | {cov}/{n} | {:.0}% | {chapter_only} |",
            show(&c.id),
            c.title.replace('|', "\\|"),
            pct(cov, n)
        );
        json_chapters.push(format!(
            "{{\"id\":{},\"title\":{},\"sections\":{n},\"covered\":{cov},\"chapter_tags\":{chapter_only},\"section_list\":[{}]}}",
            json_str(&show(&c.id)),
            json_str(&c.title),
            json_secs.join(",")
        ));
    }

    let _ = writeln!(md, "# Spec coverage\n");
    let _ = writeln!(
        md,
        "{all_cov} of {all_secs} normative sections ({:.1}%) have a tagged case or test ({all_skip} non-normative left out). {} cases, {untagged} without a `// spec:` tag; {} tagged test functions.\n",
        pct(all_cov, all_secs),
        cases.len(),
        fns.len()
    );
    let _ = writeln!(
        md,
        "## Parts\n\n| part | covered | % | non-normative |\n|---|---|---|---|"
    );
    let mut summary = String::new();
    let mut json_parts = Vec::new();
    for ((d, n), (secs, cov, skipped)) in &parts {
        let _ = writeln!(
            md,
            "| {} {n} | {cov}/{secs} | {:.0}% | {skipped} |",
            d.name(),
            pct(*cov, *secs)
        );
        let _ = writeln!(
            summary,
            "  {} {n}: {cov}/{secs} ({:.0}%), {skipped} non-normative",
            d.name(),
            pct(*cov, *secs)
        );
        json_parts.push(format!(
            "{{\"id\":\"{} {n}\",\"sections\":{secs},\"covered\":{cov},\"non_normative\":{skipped}}}",
            d.name()
        ));
    }
    let _ = writeln!(
        md,
        "\n## Chapters\n\n`chapter tags`: cases tagged with the chapter only.\n\n| chapter | title | covered | % | chapter tags |\n|---|---|---|---|---|\n{chapter_rows}"
    );
    let _ = writeln!(md, "## Non-normative sections\n");
    for (kind, ids) in NON_NORMATIVE {
        let _ = writeln!(md, "- {kind}: {ids}");
    }
    for id in &stale {
        let _ = writeln!(md, "- not in the spec (stale entry): {id}");
    }
    let _ = writeln!(md, "\n## Unresolved tags\n");
    for (tag, by) in &unresolved {
        let _ = writeln!(
            md,
            "- `{tag}`: {}",
            by.iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    let _ = writeln!(
        md,
        "\n## Sections\n\n| section | title | | cases |\n|---|---|---|---|\n{section_rows}"
    );

    let _ = writeln!(
        json,
        "  \"sections\": {all_secs},\n  \"covered\": {all_cov},\n  \"non_normative\": {all_skip},\n  \"cases\": {},\n  \"untagged_cases\": {untagged},\n  \"tagged_test_fns\": {},",
        cases.len(),
        fns.len()
    );
    let _ = writeln!(json, "  \"parts\": [{}],", json_parts.join(","));
    let _ = writeln!(
        json,
        "  \"unresolved_tags\": [{}],",
        unresolved
            .keys()
            .map(|t| json_str(t))
            .collect::<Vec<_>>()
            .join(",")
    );
    let _ = writeln!(
        json,
        "  \"chapters\": [\n    {}\n  ]\n}}",
        json_chapters.join(",\n    ")
    );

    let target = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || {
            rokc_core::real_path(common::workspace())
                .unwrap()
                .join("target")
        },
        PathBuf::from,
    );
    let _ = fs::create_dir_all(&target);
    fs::write(target.join("spec-coverage.md"), md).unwrap();
    fs::write(target.join("spec-coverage.json"), json).unwrap();
    println!(
        "spec-coverage: {all_cov}/{all_secs} normative sections ({:.1}%), {all_skip} non-normative, {} unresolved tags, {} stale non-normative entries -> {}\n{summary}",
        pct(all_cov, all_secs),
        unresolved.len(),
        stale.len(),
        target.join("spec-coverage.{md,json}").display()
    );
}

#[test]
fn tag_forms() {
    let ids = |t: &str| tag_ids(t).iter().map(show).collect::<Vec<_>>();
    assert_eq!(ids("003.05.1-tuple-position-access"), ["lang 3.5.1"]);
    assert_eq!(ids("004.12-async-basic"), ["lang 4.12"]);
    assert_eq!(ids("std 209-collections §2.9.4"), ["std 2.9", "std 2.9.4"]);
    assert_eq!(ids("std-216.5-send-sync"), ["std 2.16.5"]);
    assert_eq!(ids("std-000.03.5-user-intrinsic"), ["std 0.3.5"]);
    assert_eq!(
        ids("std 200-core/218-task §2.18.1 / refs-lang 004.12 §4.12.9"),
        ["std 2.18", "std 2.18.1", "lang 4.12", "lang 4.12.9"]
    );
    assert_eq!(
        ids("004.11-generators §4.11.3-4.11.5"),
        ["lang 4.11", "lang 4.11.3", "lang 4.11.4", "lang 4.11.5"]
    );
    let src = "// expect-stdout:\n// spec: 1 -1 0.5\n\n// spec: 003.03-let\n";
    assert_eq!(case_tags(src), ["003.03-let"]);
    let rs = "// spec: §9.1.6\n#[test]\nfn exits() {\n    // inner\n}\nfn helper() {}\n";
    assert_eq!(
        fn_tags("cli.rs", rs),
        [("cli.rs::exits".to_owned(), vec!["§9.1.6".to_owned()])]
    );
    let _ = non_normative();
}
