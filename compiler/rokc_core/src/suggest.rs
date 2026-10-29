//! "Did you mean": closest names by edit distance, and synonyms.

/// "Did you mean": the closest candidate within about a third of the
/// name's length (short names match by case only), first on ties. Exact
/// matches and internal names (`__x`, `#x`, `f/x`) give nothing.
#[must_use]
pub fn suggest<'a, I>(name: &str, candidates: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    let len = name.chars().count();
    let limit = if len < 3 { 0 } else { (len + 2) / 3 };
    let mut best: Option<(usize, &str)> = None;
    for c in candidates {
        if c == name || c.is_empty() || c.starts_with("__") || c.contains(['#', '/']) {
            continue;
        }
        let d = edit_distance(name, c);
        // A case-only difference is always worth naming.
        let d = if c.eq_ignore_ascii_case(name) { 0 } else { d };
        if d <= limit && best.is_none_or(|(b, _)| d < b) {
            best = Some((d, c));
        }
    }
    best.map(|(_, c)| c)
}

/// [`suggest`], but a name listed in `syn` (`(target, [synonyms])`)
/// gives its target first, when the target is among `candidates`.
#[must_use]
pub fn suggest_or_synonym<'a, I>(
    name: &str,
    candidates: I,
    syn: &[(&str, &[&str])],
) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    let cands: Vec<&str> = candidates.into_iter().collect();
    let hit = syn
        .iter()
        .filter(|(_, names)| names.contains(&name))
        .find_map(|(target, _)| cands.iter().find(|c| *c == target && **c != name));
    hit.copied().or_else(|| suggest(name, cands))
}

/// Levenshtein distance over chars, with adjacent transpositions
/// counted as one edit.
#[must_use]
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev2: Vec<usize> = vec![0; b.len() + 1];
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                cur[j] = cur[j].min(prev2[j - 2] + 1);
            }
        }
        prev2 = std::mem::replace(&mut prev, cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYN: &[(&str, &[&str])] = &[
        ("len", &["length", "size", "count", "lenght"]),
        ("push", &["append", "add"]),
    ];

    #[test]
    fn distance() {
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("lenght", "length"), 1);
        assert_eq!(edit_distance("same", "same"), 0);
    }

    #[test]
    fn closest_name() {
        let c = ["length", "push", "Foo", "__len", "#len"];
        assert_eq!(suggest("lenght", c), Some("length"));
        assert_eq!(suggest("foo", c), Some("Foo"));
        assert_eq!(suggest("length", c), None);
        assert_eq!(suggest("zzzzzz", c), None);
        assert_eq!(suggest("_len", ["__len"]), None);
    }

    #[test]
    fn synonyms_first_then_distance() {
        let c = ["len", "push", "pop", "clear"];
        assert_eq!(suggest_or_synonym("length", c, SYN), Some("len"));
        assert_eq!(suggest_or_synonym("size", c, SYN), Some("len"));
        assert_eq!(suggest_or_synonym("append", c, SYN), Some("push"));
        // Target missing from the candidates: no synonym hit.
        assert_eq!(suggest_or_synonym("append", ["pop"], SYN), None);
        // Not a synonym: edit distance.
        assert_eq!(suggest_or_synonym("clera", c, SYN), Some("clear"));
        assert_eq!(suggest_or_synonym("nothing", c, SYN), None);
        assert_eq!(suggest_or_synonym("len", c, SYN), None);
    }
}
