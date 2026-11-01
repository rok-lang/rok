//! `#[lang("slot")]` (spec 6.4).
//!
//! Ties a stdlib item to a compiler-known slot used to desugar tokens
//! (`+` → `add_trait::add`) and type literals (`none` → `none_type`).
//! Slots are a closed set (`KNOWN_SLOTS`); an unknown one is an error
//! (6.4.8).

use std::collections::HashMap;

use rokc_ast::{AttrArg, AttrBody, Attribute, FnDef, Item, Literal, Program};
use rokc_diagnostic::{ErrorKind, RokDiagnostic};

/// Every slot name the compiler accepts (spec 6.4.8). Source of truth:
/// stdlib spec `000-meta/004-lang-slot-table.md`; change that table first.
pub const KNOWN_SLOTS: &[&str] = &[
    // 0.4.2 type slots
    "i32_type",
    "i64_type",
    "f32_type",
    "f64_type",
    "usize_type",
    "isize_type",
    "bool_type",
    "cchar_type",
    "str_type",
    "none_type",
    "int_type",
    "float_type",
    "char_type",
    "range_type",
    // 0.4.3 trait slots
    "iterator_trait",
    "into_iterator_trait",
    "try_iterator_trait",
    "index_trait",
    "index_set_trait",
    "slice_trait",
    "display_trait",
    "debug_trait",
    "future_trait",
    // operator / comparison trait slots (0.4.3; `library/core/src/ops.rok`, `marker.rok`)
    "add_trait",
    "sub_trait",
    "mul_trait",
    "div_trait",
    "rem_trait",
    "neg_trait",
    "not_trait",
    "bitnot_trait",
    "bitand_trait",
    "bitor_trait",
    "bitxor_trait",
    "shl_trait",
    "shr_trait",
    "eq_trait",
    "cmp_trait",
    "ord_trait",
    "hash_trait",
    "clone_trait",
    // auto (structural) marker traits (`marker.rok`)
    "send_trait",
    "sync_trait",
    "logical_and_trait",
    "logical_or_trait",
    // 0.4.4 fn slots
    "panic_fn",
    "oom_fn",
    "unreachable_fn",
    // 0.4.4 fn slots of the hooks lowered code calls (`library/core/src/
    // index.rok`, `fmt.rok`, `task.rok`)
    "spread_len_fn",
    "spread_names_fn",
    "spread_misfit_fn",
    "index_from_end_fn",
    "slice_bound_fn",
    "slice_range_fn",
    "slice_mut_fn",
    "fstr_display_fn",
    "fstr_debug_fn",
    "fstr_num_fn",
    "fstr_pack_fn",
    "fstr_text_any_fn",
    "cancel_point_fn",
    "cancel_left_fn",
    "cleanup_running_fn",
    "cleanup_done_fn",
    // 0.4.5 method slots
    "drop_method",
    // 0.4.3 trait slot of a close that can fail (`library/std/src/io.rok`)
    "close_trait",
];

/// Spec 6.4.7: the method a trait slot's trait declares, as `(slot,
/// method, parameters with `self`)`.
const TRAIT_FORMS: &[(&str, &str, usize)] = &[
    ("iterator_trait", "next", 1),
    ("into_iterator_trait", "into_iter", 1),
    ("try_iterator_trait", "next", 1),
    ("index_trait", "index", 2),
    ("index_set_trait", "index_set", 3),
    ("slice_trait", "slice", 2),
    ("display_trait", "fmt", 2),
    ("debug_trait", "fmt", 2),
    ("future_trait", "poll", 2),
    ("add_trait", "add", 2),
    ("sub_trait", "sub", 2),
    ("mul_trait", "mul", 2),
    ("div_trait", "div", 2),
    ("rem_trait", "rem", 2),
    ("neg_trait", "neg", 1),
    ("not_trait", "not", 1),
    ("bitnot_trait", "bitnot", 1),
    ("bitand_trait", "bitand", 2),
    ("bitor_trait", "bitor", 2),
    ("bitxor_trait", "bitxor", 2),
    ("shl_trait", "shl", 2),
    ("shr_trait", "shr", 2),
    ("logical_and_trait", "logical_and", 2),
    ("logical_or_trait", "logical_or", 2),
    ("eq_trait", "eq", 2),
    ("cmp_trait", "lt", 2),
    ("ord_trait", "cmp", 2),
    ("hash_trait", "hash", 1),
    ("clone_trait", "clone", 1),
    ("drop_method", "drop", 1),
    ("close_trait", "close", 1),
];

/// What is wrong with `item` in `slot` (spec 6.4.7; stdlib 0.4.8: the
/// suffix names the item kind), if anything.
fn form_mismatch(slot: &str, item: &Item, method: Option<&FnDef>) -> Option<String> {
    let kind_ok = matches!(
        (slot.rsplit('_').next().unwrap_or(""), item, method),
        ("method", Item::Trait(_), Some(_))
            | ("trait", Item::Trait(_), None)
            | (
                "type",
                Item::Struct(_) | Item::Enum(_) | Item::TypeAlias(_),
                None
            )
            | ("fn", Item::Fn(_), None)
    );
    if !kind_ok {
        let want = match slot.rsplit('_').next() {
            Some("method") => "a trait method",
            Some("trait") => "a trait",
            Some("type") => "a type declaration",
            _ => "a function",
        };
        return Some(format!("goes on {want}"));
    }
    let &(_, name, arity) = TRAIT_FORMS.iter().find(|(s, ..)| *s == slot)?;
    let fits = |f: &FnDef| {
        f.name == name
            && f.params.len() == arity
            && f.params.first().is_some_and(|p| p.name == "self")
    };
    let declared = match (item, method) {
        (_, Some(f)) => fits(f),
        (Item::Trait(t), None) => t
            .items
            .iter()
            .any(|i| matches!(i, rokc_ast::TraitItem::Method(f) if fits(f))),
        _ => true,
    };
    let params = (1..arity).map(|_| ", _").collect::<String>();
    (!declared).then(|| format!("needs `fn {name}(self{params})`"))
}

/// The item kind a `#[lang]` attribute sits on, for slots that demand a
/// shape (`none_type` is a type, `add_trait` a trait).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LangItemKind {
    Trait,
    Struct,
    Enum,
    Fn,
    Const,
    Static,
    TypeAlias,
}

#[derive(Clone, Debug)]
pub struct LangEntry {
    pub slot: String,
    pub item_name: String,
    pub kind: LangItemKind,
}

#[derive(Clone, Debug, Default)]
pub struct LangTable {
    by_slot: HashMap<String, LangEntry>,
}

impl LangTable {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn lookup(&self, slot: &str) -> Option<&LangEntry> {
        self.by_slot.get(slot)
    }

    pub fn entries(&self) -> impl Iterator<Item = &LangEntry> {
        self.by_slot.values()
    }

    /// Append a pre-validated entry; the caller checks duplicates (spec
    /// 6.4.6). The driver merges per-module tables with it.
    pub fn insert_entry(&mut self, entry: LangEntry) {
        self.by_slot.insert(entry.slot.clone(), entry);
    }
}

/// Collect top-level `#[lang]` attributes; a slot used twice is an error
/// (6.4.6).
pub fn collect_lang_table(program: &Program) -> (LangTable, Vec<RokDiagnostic>) {
    collect_lang_table_onto(LangTable::default(), program)
}

/// `collect_lang_table` with `table`'s slots already bound (an earlier
/// package's).
pub fn collect_lang_table_onto(
    mut table: LangTable,
    program: &Program,
) -> (LangTable, Vec<RokDiagnostic>) {
    let mut diags = Vec::new();
    for item in &program.items {
        let (attrs, name, kind, span) = match item {
            Item::Trait(t) => (&t.attrs, &t.name, LangItemKind::Trait, t.span),
            Item::Struct(s) => (&s.attrs, &s.name, LangItemKind::Struct, s.span),
            Item::Enum(e) => (&e.attrs, &e.name, LangItemKind::Enum, e.span),
            Item::Fn(f) => (&f.attrs, &f.name, LangItemKind::Fn, f.span),
            Item::Const(c) => (&c.attrs, &c.name, LangItemKind::Const, c.span),
            Item::Static(s) => (&s.attrs, &s.name, LangItemKind::Static, s.span),
            Item::TypeAlias(a) => (&a.attrs, &a.name, LangItemKind::TypeAlias, a.span),
            _ => continue,
        };
        for attr in attrs {
            collect_attr(attr, name, kind, span, &mut table, &mut diags, (item, None));
        }
        // Method slots (`drop_method`) sit on trait methods.
        if let Item::Trait(t) = item {
            for ti in &t.items {
                if let rokc_ast::TraitItem::Method(f) = ti {
                    let qualified = format!("{}::{}", t.name, f.name);
                    for attr in &f.attrs {
                        collect_attr(
                            attr,
                            &qualified,
                            LangItemKind::Fn,
                            f.span,
                            &mut table,
                            &mut diags,
                            (item, Some(f)),
                        );
                    }
                }
            }
        }
    }
    (table, diags)
}

fn collect_attr(
    attr: &Attribute,
    item_name: &str,
    kind: LangItemKind,
    item_span: rokc_diagnostic::Span,
    table: &mut LangTable,
    diags: &mut Vec<RokDiagnostic>,
    on: (&Item, Option<&FnDef>),
) {
    if attr.name() != "lang" {
        return;
    }
    let AttrBody::Args(args) = &attr.body else {
        diags.push(RokDiagnostic::error(
            ErrorKind::Meta,
            attr.span,
            "`#[lang(...)]` requires a single string-literal slot name (spec 6.4.2)".to_owned(),
        ));
        return;
    };
    let mut slot: Option<String> = None;
    for a in args {
        match a {
            AttrArg::Lit(Literal::Str(s)) if slot.is_none() => slot = Some(s.clone()),
            _ => {}
        }
    }
    let Some(slot) = slot else {
        diags.push(RokDiagnostic::error(
            ErrorKind::Meta,
            attr.span,
            "`#[lang(...)]` requires a single string-literal slot name (spec 6.4.2)".to_owned(),
        ));
        return;
    };
    // Spec 6.4.8: unknown slot name → error.
    if !KNOWN_SLOTS.contains(&slot.as_str()) {
        diags.push(RokDiagnostic::error(
            ErrorKind::Meta,
            attr.span,
            format!("unknown lang slot `{slot}` on `{item_name}` (spec 6.4.8)"),
        ));
        return;
    }
    if let Some(why) = form_mismatch(&slot, on.0, on.1) {
        diags.push(RokDiagnostic::error(
            ErrorKind::Meta,
            attr.span,
            format!(
                "`{item_name}` does not match the form of lang slot `{slot}`: it {why} (spec 6.4.7)"
            ),
        ));
        return;
    }
    // Spec 6.4.6: same slot on two items → error.
    if let Some(prev) = table.by_slot.get(&slot) {
        diags.push(RokDiagnostic::error(
            ErrorKind::Meta,
            item_span,
            format!(
                "lang slot `{slot}` is already bound to `{}` - cannot also bind to \
                     `{item_name}` (spec 6.4.6)",
                prev.item_name
            ),
        ));
        return;
    }
    table.by_slot.insert(
        slot.clone(),
        LangEntry {
            slot,
            item_name: item_name.to_owned(),
            kind,
        },
    );
}

rokc_core::codec_enum!(LangItemKind {
    Trait,
    Struct,
    Enum,
    Fn,
    Const,
    Static,
    TypeAlias
});
rokc_core::codec_struct!(LangEntry {
    slot,
    item_name,
    kind
});
rokc_core::codec_struct!(LangTable { by_slot });
