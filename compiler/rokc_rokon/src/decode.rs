//! Typed reading for Rust consumers: the caller's target type decides
//! what a value must be. Errors are rendered (`file:line:col: error:`,
//! the line, a caret).
//!
//! ```text
//! let doc = Doc::parse(text, "rok.rokon")?;
//! let pkg = doc.root().record("Package")?;      // `Package { .. }` or `{ .. }`
//! pkg.only(&["name", "deps"])?;                // unknown fields are errors
//! let name = pkg.opt("name").map(|d| d.string()).transpose()?;
//! for (k, v) in pkg.get("deps").map(|d| d.dict()).transpose()?.unwrap_or_default() { .. }
//! ```

use crate::{Field, Kind, Span, Value};

/// A parsed file, for decoding.
pub struct Doc<'t> {
    pub text: &'t str,
    pub file: &'t str,
    pub root: Value,
}

impl<'t> Doc<'t> {
    pub fn parse(text: &'t str, file: &'t str) -> Result<Self, String> {
        let root = crate::parse(text).map_err(|e| e.render(text, file))?;
        Ok(Self { text, file, root })
    }

    pub const fn root(&self) -> Decoder<'_> {
        Decoder {
            text: self.text,
            file: self.file,
            v: &self.root,
            what: None,
        }
    }

    pub fn error(&self, span: Span, msg: &str) -> String {
        crate::render(self.text, self.file, span, "E200", msg)
    }
}

/// One value of a [`Doc`], to read as some type.
#[derive(Clone, Copy, Debug)]
pub struct Decoder<'a> {
    text: &'a str,
    file: &'a str,
    v: &'a Value,
    /// What it is for, in errors (`` `name` ``): a field's name.
    what: Option<&'a str>,
}

impl<'a> Decoder<'a> {
    pub const fn value(&self) -> &'a Value {
        self.v
    }

    pub const fn span(&self) -> Span {
        self.v.span
    }

    /// An error at this value.
    pub fn error(&self, msg: &str) -> String {
        self.error_at(self.v.span, msg)
    }

    pub fn error_at(&self, span: Span, msg: &str) -> String {
        crate::render(self.text, self.file, span, "E200", msg)
    }

    const fn child(&self, v: &'a Value, what: Option<&'a str>) -> Self {
        Self {
            text: self.text,
            file: self.file,
            v,
            what,
        }
    }

    /// "`what` must be `expected`, found .." at this value.
    pub fn expected(&self, expected: &str) -> String {
        let msg = match self.what {
            Some(w) => format!("`{w}` must be {expected}, found {}", self.v.describe()),
            None => format!("expected {expected}, found {}", self.v.describe()),
        };
        self.error(&msg)
    }

    pub const fn is_none(&self) -> bool {
        matches!(self.v.kind, Kind::None)
    }

    pub fn string(&self) -> Result<String, String> {
        match &self.v.kind {
            Kind::Str(s) => Ok(s.clone()),
            _ => Err(self.expected("a string")),
        }
    }

    pub fn bool(&self) -> Result<bool, String> {
        match self.v.kind {
            Kind::Bool(b) => Ok(b),
            _ => Err(self.expected("a bool")),
        }
    }

    /// An integer that fits `T`.
    pub fn int<T: TryFrom<i128>>(&self) -> Result<T, String> {
        match self.v.kind {
            Kind::Int(i) => T::try_from(i).map_err(|_| {
                self.error(&format!(
                    "{i} is out of range for {}",
                    std::any::type_name::<T>()
                ))
            }),
            _ => Err(self.expected("an integer")),
        }
    }

    /// A float, or an integer (as a float).
    pub fn float(&self) -> Result<f64, String> {
        match self.v.kind {
            Kind::Float(f) => Ok(f),
            Kind::Int(i) => Ok(i as f64),
            _ => Err(self.expected("a number")),
        }
    }

    pub fn char(&self) -> Result<char, String> {
        match self.v.kind {
            Kind::Char(c) => Ok(c),
            _ => Err(self.expected("a char")),
        }
    }

    pub fn list(&self) -> Result<Vec<Self>, String> {
        match &self.v.kind {
            Kind::List(vs) => Ok(vs.iter().map(|v| self.child(v, None)).collect()),
            _ => Err(self.expected("a list `[..]`")),
        }
    }

    /// A tuple of `n` values.
    pub fn tuple(&self, n: usize) -> Result<Vec<Self>, String> {
        match &self.v.kind {
            Kind::Tuple(vs) if vs.len() == n => {
                Ok(vs.iter().map(|v| self.child(v, None)).collect())
            }
            _ => Err(self.expected(&format!("a tuple of {n}"))),
        }
    }

    /// A dict's entries in order (`{}` too); duplicate keys are errors.
    pub fn dict(&self) -> Result<Vec<(Self, Self)>, String> {
        let pairs = match &self.v.kind {
            Kind::Dict(ps) => ps,
            Kind::Struct { name: None, fields } => {
                let f = &fields[0];
                return Err(self.error_at(
                    f.span,
                    &format!(
                        "expected a dict, whose keys are values: write `\"{}\": ..`",
                        f.name
                    ),
                ));
            }
            _ => return Err(self.expected("a dict `{ \"key\": value }`")),
        };
        for (i, (k, _)) in pairs.iter().enumerate() {
            if pairs[..i].iter().any(|(o, _)| o == k) {
                return Err(self.error_at(
                    k.span,
                    &format!("key {} is given twice", crate::to_string(k)),
                ));
            }
        }
        Ok(pairs
            .iter()
            .map(|(k, v)| (self.child(k, None), self.child(v, None)))
            .collect())
    }

    /// A struct `name { .. }` (the name may be left out: `{ .. }`).
    pub fn record(&self, name: &str) -> Result<Record<'a>, String> {
        let fields: &'a [Field] = match &self.v.kind {
            Kind::Struct { name: n, fields } if n.as_deref().is_none_or(|n| n == name) => fields,
            Kind::Dict(ps) if ps.is_empty() => &[],
            _ => return Err(self.expected(&format!("`{name} {{ .. }}`"))),
        };
        Record::new(*self, fields, "field")
    }

    /// A variant of enum `ty`: `ty::V`, `ty::V(..)` or `ty::V { .. }`.
    pub fn variant(&self, ty: &str) -> Result<Variant<'a>, String> {
        let Kind::Variant {
            path,
            args,
            named,
            braces,
        } = &self.v.kind
        else {
            return Err(self.expected(&format!("a `{ty}::..` variant")));
        };
        let name = match path.split_once("::") {
            Some((t, v)) if t == ty && !v.contains("::") => v,
            _ => return Err(self.expected(&format!("a `{ty}::..` variant"))),
        };
        Ok(Variant {
            name,
            args: args.iter().map(|v| self.child(v, None)).collect(),
            fields: Record::new(*self, named, if *braces { "field" } else { "argument" })?,
            decoder: *self,
        })
    }
}

/// A struct's fields (or a variant's named arguments), checked unique.
#[derive(Clone, Debug)]
pub struct Record<'a> {
    d: Decoder<'a>,
    fields: &'a [Field],
    /// "field" / "argument", in errors.
    noun: &'static str,
}

impl<'a> Record<'a> {
    fn new(d: Decoder<'a>, fields: &'a [Field], noun: &'static str) -> Result<Self, String> {
        for (i, f) in fields.iter().enumerate() {
            if fields[..i].iter().any(|o| o.name == f.name) {
                return Err(d.error_at(f.span, &format!("{noun} `{}` is given twice", f.name)));
            }
        }
        Ok(Self { d, fields, noun })
    }

    /// The whole struct.
    pub const fn decoder(&self) -> Decoder<'a> {
        self.d
    }

    /// The field's names, in order.
    pub fn names(&self) -> impl Iterator<Item = &'a str> {
        self.fields.iter().map(|f| f.name.as_str())
    }

    /// Field `name`, if written.
    pub fn get(&self, name: &str) -> Option<Decoder<'a>> {
        self.fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| self.d.child(&f.value, Some(&f.name)))
    }

    /// Field `name` of a `T | none` type: missing or `none` is `None`.
    pub fn opt(&self, name: &str) -> Option<Decoder<'a>> {
        self.get(name).filter(|d| !d.is_none())
    }

    /// Field `name`, which must be written.
    pub fn req(&self, name: &str) -> Result<Decoder<'a>, String> {
        self.get(name)
            .ok_or_else(|| self.d.error(&format!("missing {} `{name}`", self.noun)))
    }

    /// An error at field `name`'s name (at the struct when missing).
    pub fn field_error(&self, name: &str, msg: &str) -> String {
        let span = self
            .fields
            .iter()
            .find(|f| f.name == name)
            .map_or(self.d.span(), |f| f.span);
        self.d.error_at(span, msg)
    }

    /// Every field is one of `known`.
    pub fn only(&self, known: &[&str]) -> Result<(), String> {
        match self
            .fields
            .iter()
            .find(|f| !known.contains(&f.name.as_str()))
        {
            Some(f) => Err(self.d.error_at(
                f.span,
                &format!(
                    "unknown {} `{}` (expected {})",
                    self.noun,
                    f.name,
                    known.join(", ")
                ),
            )),
            None => Ok(()),
        }
    }

    /// The fields not in `known`, in order, for files several toolchains
    /// share: a newer one may have written them, so they are skipped.
    pub fn known(&self, known: &[&str]) -> Vec<&'a str> {
        self.names().filter(|n| !known.contains(n)).collect()
    }
}

/// An enum variant: its name (after `Type::`), positional arguments and
/// named arguments / fields.
pub struct Variant<'a> {
    pub name: &'a str,
    pub args: Vec<Decoder<'a>>,
    pub fields: Record<'a>,
    /// The whole variant.
    pub decoder: Decoder<'a>,
}
