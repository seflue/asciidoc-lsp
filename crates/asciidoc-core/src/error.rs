use thiserror::Error;

/// Typed errors for asciidoc-core.
#[derive(Debug, Error)]
pub enum CoreError {
    /// Failed to load the tree-sitter language.
    #[error("failed to initialize tree-sitter language: {0}")]
    LanguageInit(#[from] tree_sitter::LanguageError),

    /// Tree-sitter Parser gab kein Ergebnis zurück.
    #[error("tree-sitter failed to produce a parse tree")]
    ParseFailed,
}
