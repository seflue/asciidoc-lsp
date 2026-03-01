use tower_lsp_server::ls_types;

use asciidoc_core::types::{Diagnostic as CoreDiagnostic, DiagnosticKind};

use crate::positions;

/// Convert core diagnostics (for a single file) into LSP diagnostics.
///
/// `diags` must be pre-filtered for a single file.
/// `content` is the text content of that file (for line-to-position conversion).
pub fn convert_diagnostics(diags: &[CoreDiagnostic], content: &str) -> Vec<ls_types::Diagnostic> {
    diags.iter().map(|d| convert_one(d, content)).collect()
}

fn convert_one(diag: &CoreDiagnostic, content: &str) -> ls_types::Diagnostic {
    let start = positions::to_lsp_position(diag.line, 0, content);

    let line_text = content
        .lines()
        .nth(diag.line.saturating_sub(1))
        .unwrap_or_default();
    let end = positions::to_lsp_position(diag.line, line_text.len(), content);

    let severity = match &diag.kind {
        DiagnosticKind::BrokenInclude { .. } => ls_types::DiagnosticSeverity::ERROR,
        DiagnosticKind::BrokenXref { .. }
        | DiagnosticKind::DuplicateAnchor { .. }
        | DiagnosticKind::UndefinedAttribute { .. } => ls_types::DiagnosticSeverity::WARNING,
    };

    ls_types::Diagnostic::new(
        ls_types::Range { start, end },
        Some(severity),
        None,
        Some("asciidoc-lsp".into()),
        diag.message.clone(),
        None,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_convert_broken_xref_is_warning() {
        let content = "= Title\n\nSee <<missing>>.\n";
        let diags = vec![CoreDiagnostic {
            file: PathBuf::from("/test.adoc"),
            line: 3,
            kind: DiagnosticKind::BrokenXref {
                target: "missing".into(),
            },
            message: "Broken cross-reference: missing".into(),
        }];

        let result = convert_diagnostics(&diags, content);

        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].severity,
            Some(ls_types::DiagnosticSeverity::WARNING)
        );
        assert_eq!(result[0].range.start.line, 2); // 0-indexed
        assert_eq!(result[0].source.as_deref(), Some("asciidoc-lsp"));
    }

    #[test]
    fn test_convert_broken_include_is_error() {
        let content = "= Title\n\ninclude::missing.adoc[]\n";
        let diags = vec![CoreDiagnostic {
            file: PathBuf::from("/test.adoc"),
            line: 3,
            kind: DiagnosticKind::BrokenInclude {
                path: "missing.adoc".into(),
            },
            message: "Broken include: missing.adoc".into(),
        }];

        let result = convert_diagnostics(&diags, content);

        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].severity,
            Some(ls_types::DiagnosticSeverity::ERROR)
        );
    }

    #[test]
    fn test_convert_duplicate_anchor_is_warning() {
        let content = "[[dup]]\n== Section\n";
        let diags = vec![CoreDiagnostic {
            file: PathBuf::from("/test.adoc"),
            line: 1,
            kind: DiagnosticKind::DuplicateAnchor {
                id: "dup".into(),
                first_file: PathBuf::from("/other.adoc"),
            },
            message: "Duplicate anchor 'dup'".into(),
        }];

        let result = convert_diagnostics(&diags, content);

        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].severity,
            Some(ls_types::DiagnosticSeverity::WARNING)
        );
    }

    #[test]
    fn test_convert_undefined_attribute_is_warning() {
        let content = "= Title\n\n{undefined}\n";
        let diags = vec![CoreDiagnostic {
            file: PathBuf::from("/test.adoc"),
            line: 3,
            kind: DiagnosticKind::UndefinedAttribute {
                name: "undefined".into(),
            },
            message: "Undefined attribute: {undefined}".into(),
        }];

        let result = convert_diagnostics(&diags, content);

        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].severity,
            Some(ls_types::DiagnosticSeverity::WARNING)
        );
    }

    #[test]
    fn test_convert_empty_diagnostics() {
        let content = "= Title\n";
        let diags: Vec<CoreDiagnostic> = vec![];

        let result = convert_diagnostics(&diags, content);

        assert!(result.is_empty());
    }

    #[test]
    fn test_convert_range_covers_line() {
        let content = "= Title\n\nSee <<missing>>.\n";
        let diags = vec![CoreDiagnostic {
            file: PathBuf::from("/test.adoc"),
            line: 3,
            kind: DiagnosticKind::BrokenXref {
                target: "missing".into(),
            },
            message: "Broken cross-reference: missing".into(),
        }];

        let result = convert_diagnostics(&diags, content);

        assert_eq!(result[0].range.start.line, 2);
        assert_eq!(result[0].range.start.character, 0);
        assert_eq!(result[0].range.end.line, 2);
        // "See <<missing>>." = 16 chars
        assert_eq!(result[0].range.end.character, 16);
    }

    #[test]
    fn test_convert_multiple_diagnostics() {
        let content = "= Title\n\nSee <<a>>.\n\nSee <<b>>.\n";
        let diags = vec![
            CoreDiagnostic {
                file: PathBuf::from("/test.adoc"),
                line: 3,
                kind: DiagnosticKind::BrokenXref { target: "a".into() },
                message: "Broken cross-reference: a".into(),
            },
            CoreDiagnostic {
                file: PathBuf::from("/test.adoc"),
                line: 5,
                kind: DiagnosticKind::BrokenXref { target: "b".into() },
                message: "Broken cross-reference: b".into(),
            },
        ];

        let result = convert_diagnostics(&diags, content);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].range.start.line, 2);
        assert_eq!(result[1].range.start.line, 4);
    }
}
