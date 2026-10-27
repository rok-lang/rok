//! Parser: tokens to AST. Precedence is fixed by the language (spec
//! 3.2.3, `prec.rs`); `#[infix]` / `#[prefix]` / `#[postfix]` only bind
//! tokens to trait methods (`PrecTable`).

pub mod cfg;
pub mod derive;
pub mod keyword;
pub mod lang;
pub mod parser;
pub mod prec;

#[cfg(test)]
mod tests;

pub use cfg::CfgEnv;
pub use keyword::{KeywordDef, KeywordTable, apply_keywords, check_keyword, collect_keywords};
pub use lang::{LangEntry, LangItemKind, LangTable, collect_lang_table, collect_lang_table_onto};
pub use parser::expand_to_text;
pub use parser::literal::{
    ArgKind, LitRule, LiteralDef, LiteralTable, PatElem, PatKind, Pattern, Shape, collect_literals,
    parse_pattern, user_suffix,
};
pub use parser::meta::{
    BUILTIN_ATTRS, MetaDef, MetaKind, MetaTable, PKG_ROOT, collect_meta_blocks,
};
pub use parser::{
    Parser, VARIADIC_SLICE, include_path, parse_expr_file, parse_program, parse_program_ext,
    parse_program_meta,
};
pub use prec::{Assoc, ExtOps, Fixity, OpEntry, PrecTable, collect_prec_table};
