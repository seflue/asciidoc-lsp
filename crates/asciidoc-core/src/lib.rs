pub mod anchor_scope;
pub mod attributes;
pub mod builtin_attributes;
pub mod diagnostics;
pub mod error;
pub mod extractors;
pub mod graph;
pub mod parser;
pub mod paths;
mod pdf_header;
pub mod pdf_theme;
mod pdf_theme_yaml;
pub mod quick_fix;
mod reading;
pub mod references;
mod section_levels;
pub mod source_text;
pub mod substituted_refs;
pub mod types;
pub mod ventilate;
pub mod workspace;

/// The committed fixture `tests/fixtures/symlinks` of the repository,
/// canonical. See the "Committed fixtures" section in the LSP's tests.
#[cfg(test)]
pub(crate) fn symlink_fixture() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/symlinks")
        .canonicalize()
        .expect("symlink fixture should exist")
}
