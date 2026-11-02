//! `cfg` predicates: the environment and its evaluation,
//! shared by item filtering (driver), `#[cfg_attr]` and `cfg!(..)`
//! (parser).

use std::collections::BTreeSet;

use rokc_ast::{AttrArg, AttrBody, Attribute, Literal};

/// Bare flags (`test`, `debug_assertions`) and `key = "value"` pairs
/// (`target_os = "macos"`).
#[derive(Clone, Debug, Default)]
pub struct CfgEnv {
    flags: BTreeSet<String>,
    pairs: BTreeSet<(String, String)>,
}

impl CfgEnv {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A `--cfg` argument: `flag` or `key=value` (quotes optional).
    pub fn add(&mut self, raw: &str) {
        match raw.split_once('=') {
            Some((key, value)) => self.add_pair(key.trim(), value.trim().trim_matches('"')),
            None => {
                self.flags.insert(raw.trim().to_owned());
            }
        }
    }

    pub fn add_pair(&mut self, key: &str, value: &str) {
        self.pairs.insert((key.to_owned(), value.to_owned()));
    }

    #[must_use]
    pub fn has_flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }

    /// One line per active cfg, flags first (`rokc --print cfg`).
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut out: Vec<String> = self.flags.iter().cloned().collect();
        out.extend(self.pairs.iter().map(|(k, v)| format!("{k}=\"{v}\"")));
        out
    }

    /// Whether one predicate (`test`, `k = "v"`, `all(..)`, `any(..)`,
    /// `not(..)`) holds.
    #[must_use]
    pub fn eval(&self, arg: &AttrArg) -> bool {
        match arg {
            AttrArg::Word(name) => self.flags.contains(name),
            AttrArg::KeyValue(key, Literal::Str(v) | Literal::RawStr(v)) => {
                self.pairs.contains(&(key.clone(), v.clone()))
            }
            AttrArg::Nested { path, body } => match (path.as_slice(), body.as_ref()) {
                ([p], AttrBody::Args(xs)) if p == "all" => xs.iter().all(|a| self.eval(a)),
                ([p], AttrBody::Args(xs)) if p == "any" => xs.iter().any(|a| self.eval(a)),
                ([p], AttrBody::Args(xs)) if p == "not" && xs.len() == 1 => !self.eval(&xs[0]),
                _ => false,
            },
            _ => false,
        }
    }

    /// Whether `#[cfg(p)]` holds (malformed: checked by `check_cfg`).
    #[must_use]
    pub fn eval_attr(&self, a: &Attribute) -> bool {
        match &a.body {
            AttrBody::Args(args) if args.len() == 1 => self.eval(&args[0]),
            _ => true,
        }
    }

    /// Whether every `#[cfg(..)]` among `attrs` holds.
    #[must_use]
    pub fn attrs_pass(&self, attrs: &[Attribute]) -> bool {
        attrs.iter().all(|a| a.path != ["cfg"] || self.eval_attr(a))
    }
}

/// Why `arg` is not a well-formed predicate, if it is not.
#[must_use]
pub fn check_pred(arg: &AttrArg) -> Option<String> {
    match arg {
        AttrArg::Word(_) | AttrArg::KeyValue(_, Literal::Str(_) | Literal::RawStr(_)) => None,
        AttrArg::KeyValue(k, _) => Some(format!("the value of cfg key `{k}` must be a string")),
        AttrArg::Nested { path, body } => match (path.as_slice(), body.as_ref()) {
            ([p], AttrBody::Args(xs)) if p == "all" || p == "any" => xs.iter().find_map(check_pred),
            ([p], AttrBody::Args(xs)) if p == "not" => {
                if xs.len() == 1 {
                    check_pred(&xs[0])
                } else {
                    Some("`not(..)` takes exactly one predicate".to_owned())
                }
            }
            _ => Some(format!(
                "unknown cfg combinator `{}` (`all`, `any`, `not`)",
                path.join("::")
            )),
        },
        AttrArg::Lit(_) | AttrArg::Range { .. } => {
            Some("a cfg predicate is a flag, `key = \"value\"` or `all` / `any` / `not`".to_owned())
        }
    }
}

/// `#[cfg_attr(p, a, b)]` → `a`, `b` when `p` holds, nothing otherwise.
/// Other attributes pass through.
pub fn expand_cfg_attr(env: &CfgEnv, a: Attribute, out: &mut Vec<Attribute>) {
    if a.path != ["cfg_attr"] {
        out.push(a);
        return;
    }
    let AttrBody::Args(args) = &a.body else {
        out.push(a);
        return;
    };
    let [pred, rest @ ..] = args.as_slice() else {
        out.push(a);
        return;
    };
    if rest.is_empty() || check_pred(pred).is_some() {
        out.push(a);
        return;
    }
    if !env.eval(pred) {
        return;
    }
    for r in rest {
        let (path, body) = match r {
            AttrArg::Word(w) => (vec![w.clone()], AttrBody::Empty),
            AttrArg::Nested { path, body } => (path.clone(), (**body).clone()),
            AttrArg::KeyValue(k, v) => (vec![k.clone()], AttrBody::Value(v.clone())),
            AttrArg::Lit(_) | AttrArg::Range { .. } => continue,
        };
        expand_cfg_attr(
            env,
            Attribute {
                path,
                body,
                inner: a.inner,
                span: a.span,
            },
            out,
        );
    }
}

/// What is wrong with `#[cfg(..)]` attribute `a`, if anything.
#[must_use]
pub fn attr_problem(a: &Attribute) -> Option<String> {
    match &a.body {
        AttrBody::Args(xs) if xs.len() == 1 => check_pred(&xs[0]),
        _ => Some("`#[cfg]` takes exactly one predicate".to_owned()),
    }
}
