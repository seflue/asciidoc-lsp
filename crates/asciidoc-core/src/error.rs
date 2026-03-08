use thiserror::Error;

/// Typisierte Fehler für asciidoc-core.
#[derive(Debug, Error)]
pub enum CoreError {
    /// Tree-sitter Language konnte nicht geladen werden.
    #[error("failed to initialize tree-sitter language: {0}")]
    LanguageInit(#[from] tree_sitter::LanguageError),

    /// Tree-sitter Parser gab kein Ergebnis zurück.
    #[error("tree-sitter failed to produce a parse tree")]
    ParseFailed,
}
