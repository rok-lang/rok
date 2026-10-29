//! Tools on the compiler front end: language server, REPL and Jupyter
//! kernel, linter, doc generator and C binding generator. Each takes its
//! configuration as arguments, so any CLI front can host them.

pub mod analysis;
pub mod bindgen;
mod complete;
mod copy_write;
pub mod doc;
pub mod jupyter;
mod line_edit;
pub mod lint;
pub mod lsp;
mod members;
pub mod repl;

#[cfg(test)]
mod lsp_tests;
