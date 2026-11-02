//! A dependency's `target`: a target name (`x86_64-unknown-linux-gnu`)
//! or `cfg(<predicate>)` over the target's keys, as in
//! `#[cfg(..)]`: `target_os = "linux"`, `all(..)`, `any(..)`, `not(..)`.

use rokc_target::TargetSpec;

#[derive(Debug, PartialEq, Eq)]
enum Pred {
    Key(String, String),
    All(Vec<Pred>),
    Any(Vec<Pred>),
    Not(Box<Pred>),
}

/// The target keys a predicate may test.
const KEYS: [&str; 6] = [
    "target_os",
    "target_arch",
    "target_family",
    "target_env",
    "target_endian",
    "target_pointer_width",
];

const FORMS: &str = "a target is a target name (`x86_64-unknown-linux-gnu`) or \
                     `cfg(target_os = \"linux\")`, with `all(..)`, `any(..)`, `not(..)`";

struct Parser<'a> {
    s: &'a str,
}

impl Parser<'_> {
    fn ws(&mut self) {
        self.s = self.s.trim_start();
    }

    fn eat(&mut self, c: char) -> bool {
        self.ws();
        match self.s.strip_prefix(c) {
            Some(r) => {
                self.s = r;
                true
            }
            None => false,
        }
    }

    fn ident(&mut self) -> Option<&str> {
        self.ws();
        let n = self
            .s
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(self.s.len());
        let (id, rest) = self.s.split_at(n);
        self.s = rest;
        (!id.is_empty()).then_some(id)
    }

    fn string(&mut self) -> Option<String> {
        if !self.eat('"') {
            return None;
        }
        let end = self.s.find('"')?;
        let v = self.s[..end].to_owned();
        self.s = &self.s[end + 1..];
        Some(v)
    }

    fn pred(&mut self) -> Result<Pred, String> {
        let id = self.ident().ok_or(FORMS)?.to_owned();
        if matches!(id.as_str(), "all" | "any" | "not") {
            if !self.eat('(') {
                return Err(format!("`{id}` takes `(..)`"));
            }
            let mut xs = Vec::new();
            while !self.eat(')') {
                xs.push(self.pred()?);
                if !self.eat(',') {
                    if !self.eat(')') {
                        return Err(format!("`{id}(..)` is not closed"));
                    }
                    break;
                }
            }
            return match id.as_str() {
                "all" => Ok(Pred::All(xs)),
                "any" => Ok(Pred::Any(xs)),
                _ if xs.len() == 1 => Ok(Pred::Not(Box::new(xs.remove(0)))),
                _ => Err("`not(..)` takes exactly one predicate".to_owned()),
            };
        }
        if !KEYS.contains(&id.as_str()) {
            return Err(format!(
                "`{id}` is not a target key (expected {})",
                KEYS.join(", ")
            ));
        }
        if !self.eat('=') {
            return Err(format!("write `{id} = \"..\"`"));
        }
        let v = self
            .string()
            .ok_or_else(|| format!("the value of `{id}` is a string"))?;
        Ok(Pred::Key(id, v))
    }
}

fn parse(s: &str) -> Result<Option<Pred>, String> {
    let Some(inner) = s.trim().strip_prefix("cfg(") else {
        let name = s.trim();
        let ok = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        return if ok { Ok(None) } else { Err(FORMS.to_owned()) };
    };
    let mut p = Parser { s: inner };
    let pred = p.pred()?;
    if !p.eat(')') || !p.s.trim().is_empty() {
        return Err(format!("`{s}`: {FORMS}"));
    }
    Ok(Some(pred))
}

/// Whether `s` is a target or a well-formed `cfg(..)`.
pub fn check(s: &str) -> Result<(), String> {
    parse(s).map(drop)
}

fn eval(p: &Pred, t: &TargetSpec) -> bool {
    match p {
        Pred::Key(k, v) => {
            let have = match k.as_str() {
                "target_os" => t.os.clone(),
                "target_arch" => t.arch.clone(),
                "target_family" => t.family.clone(),
                "target_env" => t.env.clone(),
                "target_endian" => t.endian.clone(),
                _ => t.pointer_width.to_string(),
            };
            have == *v
        }
        Pred::All(xs) => xs.iter().all(|x| eval(x, t)),
        Pred::Any(xs) => xs.iter().any(|x| eval(x, t)),
        Pred::Not(x) => !eval(x, t),
    }
}

/// Whether a build for `t` uses a dependency with `target` (none: every
/// build does).
pub fn matches(target: Option<&str>, t: &TargetSpec) -> bool {
    let Some(s) = target else {
        return true;
    };
    match parse(s) {
        Ok(Some(p)) => eval(&p, t),
        Ok(None) => s.trim() == t.name,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_and_predicates() {
        let linux = TargetSpec::builtin("x86_64-unknown-linux-gnu").unwrap();
        let win = TargetSpec::builtin("x86_64-pc-windows-msvc").unwrap();
        let unix = "cfg(target_family = \"unix\")";
        assert!(matches(Some(unix), &linux) && !matches(Some(unix), &win));
        let p = "cfg(all(not(target_os = \"macos\"), any(target_env = \"gnu\", target_env = \"musl\")))";
        assert!(matches(Some(p), &linux) && !matches(Some(p), &win));
        assert!(matches(Some("x86_64-pc-windows-msvc"), &win));
        assert!(!matches(Some("x86_64-pc-windows-msvc"), &linux));
        assert!(matches(None, &win));
        assert!(matches(Some("cfg(target_pointer_width = \"64\")"), &linux));
        for bad in [
            "cfg(unix)",
            "cfg(target_os)",
            "cfg(not(a, b))",
            "cfg(any(",
            "a b",
            "",
        ] {
            assert!(check(bad).is_err(), "{bad}");
        }
    }
}
