pub mod anchor_scope;
pub mod attributes;
pub mod diagnostics;
pub mod error;
pub mod extractors;
pub mod graph;
pub mod parser;
pub mod paths;
pub mod pdf_theme;
pub mod quick_fix;
mod reading;
pub mod references;
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
