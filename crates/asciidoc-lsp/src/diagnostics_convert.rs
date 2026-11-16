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
    let line_text = content
        .lines()
        .nth(diag.line.saturating_sub(1))
        .unwrap_or_default();
    let (start_col, end_col) = diag.span.unwrap_or((0, line_text.len()));
    let start = positions::to_lsp_position(diag.line, start_col, content);
    let end = positions::to_lsp_position(diag.line, end_col, content);

    let severity = match &diag.kind {
        DiagnosticKind::BrokenInclude { .. }
        | DiagnosticKind::PdfPathNotFound { .. }
        | DiagnosticKind::PdfThemeYamlExtension { .. }
        | DiagnosticKind::UnknownPdfTheme { .. } => ls_types::DiagnosticSeverity::ERROR,
        DiagnosticKind::BrokenXref { .. }
        | DiagnosticKind::DuplicateAnchor { .. }
        | DiagnosticKind::UndefinedAttribute { .. }
        | DiagnosticKind::CircularInclude { .. }
        | DiagnosticKind::UnanchoredPdfPath { .. } => ls_types::DiagnosticSeverity::WARNING,
        DiagnosticKind::UndefinedConditionalAttr { .. } => ls_types::DiagnosticSeverity::HINT,
    };

    ls_types::Diagnostic::new(
        ls_types::Range { start, end },
        Some(severity),
        None,
        Some("asciidoc-lsp".into()),
        diag.kind.to_string(),
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
            span: None,
            kind: DiagnosticKind::BrokenXref {
                target: "missing".into(),
            },
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
            span: None,
            kind: DiagnosticKind::BrokenInclude {
                path: "missing.adoc".into(),
            },
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
            span: None,
            kind: DiagnosticKind::DuplicateAnchor {
                id: "dup".into(),
                first_file: PathBuf::from("/other.adoc"),
                first_line: 1,
            },
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
            span: None,
            kind: DiagnosticKind::UndefinedAttribute {
                name: "undefined".into(),
            },
        }];

        let result = convert_diagnostics(&diags, content);

        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].severity,
            Some(ls_types::DiagnosticSeverity::WARNING)
        );
    }

    #[test]
    fn test_convert_circular_include_is_warning() {
        let content = "= Title\n\ninclude::b.adoc[]\n";
        let diags = vec![CoreDiagnostic {
            file: PathBuf::from("/test.adoc"),
            line: 3,
            span: None,
            kind: DiagnosticKind::CircularInclude {
                path: "b.adoc".into(),
                cycle_through: PathBuf::from("/b.adoc"),
            },
        }];

        let result = convert_diagnostics(&diags, content);

        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].severity,
            Some(ls_types::DiagnosticSeverity::WARNING)
        );
    }

    #[test]
    fn test_convert_undefined_conditional_attr_is_hint() {
        let content = "= Title\n\nifdef::missing[]\nContent.\nendif::[]\n";
        let diags = vec![CoreDiagnostic {
            file: PathBuf::from("/test.adoc"),
            line: 3,
            span: None,
            kind: DiagnosticKind::UndefinedConditionalAttr {
                name: "missing".into(),
                directive: "ifdef".into(),
            },
        }];

        let result = convert_diagnostics(&diags, content);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].severity, Some(ls_types::DiagnosticSeverity::HINT));
    }

    #[test]
    fn test_convert_pdf_path_severities() {
        let content = ":pdf-theme: custom\n";
        let attribute = || "pdf-theme".to_string();
        for (kind, severity) in [
            (
                DiagnosticKind::PdfPathNotFound {
                    attribute: attribute(),
                    path: PathBuf::from("/themes/custom-theme.yml"),
                },
                ls_types::DiagnosticSeverity::ERROR,
            ),
            (
                DiagnosticKind::PdfThemeYamlExtension {
                    value: "custom.yaml".into(),
                },
                ls_types::DiagnosticSeverity::ERROR,
            ),
            (
                DiagnosticKind::UnknownPdfTheme {
                    name: "custom".into(),
                },
                ls_types::DiagnosticSeverity::ERROR,
            ),
            (
                DiagnosticKind::UnanchoredPdfPath {
                    attribute: attribute(),
                    value: "custom.yml".into(),
                },
                ls_types::DiagnosticSeverity::WARNING,
            ),
        ] {
            let diags = vec![CoreDiagnostic {
                file: PathBuf::from("/test.adoc"),
                line: 1,
                span: None,
                kind: kind.clone(),
            }];
            let result = convert_diagnostics(&diags, content);
            assert_eq!(result[0].severity, Some(severity), "{kind:?}");
        }
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
            span: None,
            kind: DiagnosticKind::BrokenXref {
                target: "missing".into(),
            },
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
                span: None,
                kind: DiagnosticKind::BrokenXref { target: "a".into() },
            },
            CoreDiagnostic {
                file: PathBuf::from("/test.adoc"),
                line: 5,
                span: None,
                kind: DiagnosticKind::BrokenXref { target: "b".into() },
            },
        ];

        let result = convert_diagnostics(&diags, content);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].range.start.line, 2);
        assert_eq!(result[1].range.start.line, 4);
    }

    /// Text covered by a single-line LSP range (UTF-16 columns).
    fn range_text(content: &str, range: ls_types::Range) -> String {
        assert_eq!(range.start.line, range.end.line);
        let line = content.lines().nth(range.start.line as usize).unwrap();
        let units: Vec<u16> = line.encode_utf16().collect();
        String::from_utf16(&units[range.start.character as usize..range.end.character as usize])
            .unwrap()
    }

    /// Three broken xrefs on one line (after a multibyte prefix) and a broken
    /// include: every diagnostic underlines exactly its own macro, ranges are
    /// disjoint (committed fixture `tests/fixtures/diagnostic_ranges`).
    #[test]
    fn test_fixture_broken_refs_underline_only_themselves() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/diagnostic_ranges")
            .canonicalize()
            .unwrap();
        let main = root.join("main.adoc");
        let content = std::fs::read_to_string(&main).unwrap();
        let ws = asciidoc_core::workspace::Workspace::build(&root, &main);

        let result = convert_diagnostics(ws.diagnostics_for(&main), &content);

        let mut texts: Vec<(u32, String)> = result
            .iter()
            .map(|d| (d.range.start.character, range_text(&content, d.range)))
            .collect();
        texts.sort();
        let texts: Vec<String> = texts.into_iter().map(|(_, t)| t).collect();
        assert_eq!(
            texts,
            vec![
                "include::gone-include.adoc[]",
                "<<gone-a>>",
                "xref:gone-b.adoc[B]",
                "<<gone-c,C>>",
            ]
        );
    }
}
