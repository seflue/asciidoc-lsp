use tower_lsp_server::ls_types;

use asciidoc_core::types::{Diagnostic as CoreDiagnostic, DiagnosticKind, Span};

use crate::positions;

/// Convert core diagnostics (for a single file) into LSP diagnostics.
///
/// `diags` must be pre-filtered for a single file.
/// `content` is the text content of that file (for line-to-position conversion).
pub fn convert_diagnostics(diags: &[CoreDiagnostic], content: &str) -> Vec<ls_types::Diagnostic> {
    diags.iter().map(|d| convert_one(d, content)).collect()
}

pub(crate) fn convert_one(diag: &CoreDiagnostic, content: &str) -> ls_types::Diagnostic {
    let line_text = content
        .lines()
        .nth(diag.line.saturating_sub(1))
        .unwrap_or_default();
    let span = diag.span.unwrap_or(Span {
        start_col: 0,
        end_line: diag.line,
        end_col: line_text.len(),
    });
    let start = positions::to_lsp_position(diag.line, span.start_col, content);
    let end = positions::to_lsp_position(span.end_line, span.end_col, content);

    let severity = match &diag.kind {
        DiagnosticKind::BrokenInclude { .. }
        | DiagnosticKind::PdfPathNotFound { .. }
        | DiagnosticKind::PdfThemeYamlExtension { .. }
        | DiagnosticKind::UnknownPdfTheme { .. }
        | DiagnosticKind::LevelZeroSectionOutsideBook { .. } => ls_types::DiagnosticSeverity::ERROR,
        DiagnosticKind::BrokenXref { .. }
        | DiagnosticKind::DuplicateAnchor { .. }
        | DiagnosticKind::IncludeTagNotFound { .. }
        | DiagnosticKind::UndefinedAttribute { .. }
        | DiagnosticKind::CircularInclude { .. }
        | DiagnosticKind::SectionOutOfSequence { .. }
        | DiagnosticKind::UnterminatedBlock { .. }
        | DiagnosticKind::ImageNotFound { .. }
        | DiagnosticKind::UnanchoredPdfPath { .. } => ls_types::DiagnosticSeverity::WARNING,
        DiagnosticKind::OptionalIncludeNotFound { .. } => ls_types::DiagnosticSeverity::INFORMATION,
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
    fn test_convert_image_not_found_is_warning() {
        let diags = vec![CoreDiagnostic {
            file: PathBuf::from("/test.adoc"),
            line: 1,
            span: None,
            kind: DiagnosticKind::ImageNotFound {
                path: "a.png".into(),
                missing: vec![],
            },
        }];

        let result = convert_diagnostics(&diags, "image::a.png[]\n");

        assert_eq!(
            result[0].severity,
            Some(ls_types::DiagnosticSeverity::WARNING)
        );
    }

    #[test]
    fn test_convert_broken_xref_is_warning() {
        let content = "= Title\n\nSee <<missing>>.\n";
        let diags = vec![CoreDiagnostic {
            file: PathBuf::from("/test.adoc"),
            line: 3,
            span: None,
            kind: DiagnosticKind::BrokenXref {
                target: "missing".into(),
                documents: vec![],
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
    fn test_convert_optional_include_not_found_is_information() {
        let content = "= Title\n\ninclude::missing.adoc[opts=optional]\n";
        let diags = vec![CoreDiagnostic {
            file: PathBuf::from("/test.adoc"),
            line: 3,
            span: None,
            kind: DiagnosticKind::OptionalIncludeNotFound {
                path: "missing.adoc".into(),
            },
        }];

        let result = convert_diagnostics(&diags, content);

        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].severity,
            Some(ls_types::DiagnosticSeverity::INFORMATION)
        );
        assert_eq!(
            result[0].message,
            "Optional include not found: missing.adoc"
        );
    }

    #[test]
    fn test_convert_include_tag_not_found_is_warning() {
        let content = "= Title\n\ninclude::a.yml[tag=x]\n";
        let diags = vec![CoreDiagnostic {
            file: PathBuf::from("/test.adoc"),
            line: 3,
            span: None,
            kind: DiagnosticKind::IncludeTagNotFound {
                tags: vec!["x".into()],
                path: "a.yml".into(),
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
    fn test_convert_section_out_of_sequence_is_warning() {
        let content = "= Title\n\n== A\n\n==== B\n";
        let diags = vec![CoreDiagnostic {
            file: PathBuf::from("/test.adoc"),
            line: 5,
            span: None,
            kind: DiagnosticKind::SectionOutOfSequence {
                expected: vec![3],
                got: 4,
                documents: vec![],
            },
        }];

        let result = convert_diagnostics(&diags, content);

        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].severity,
            Some(ls_types::DiagnosticSeverity::WARNING)
        );
        assert_eq!(result[0].range.start.line, 4);
    }

    /// Asciidoctor reports it as an `ERROR`.
    #[test]
    fn test_convert_level_zero_section_outside_book_is_error() {
        let content = "= Title\n\n= Part\n";
        let diags = vec![CoreDiagnostic {
            file: PathBuf::from("/test.adoc"),
            line: 3,
            span: None,
            kind: DiagnosticKind::LevelZeroSectionOutsideBook { documents: vec![] },
        }];

        let result = convert_diagnostics(&diags, content);

        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].severity,
            Some(ls_types::DiagnosticSeverity::ERROR)
        );
        assert_eq!(result[0].range.start.line, 2);
    }

    #[test]
    fn test_convert_unterminated_block_is_warning() {
        let content = "para\n\n----\ncode\n";
        let diags = vec![CoreDiagnostic {
            file: PathBuf::from("/test.adoc"),
            line: 3,
            span: None,
            kind: DiagnosticKind::UnterminatedBlock {
                context: "listing".into(),
                delimiter: "----".into(),
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
                documents: vec![PathBuf::from("/test.adoc")],
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
            span: Some(Span {
                start_col: 0,
                end_line: 3,
                end_col: 11,
            }),
            kind: DiagnosticKind::UndefinedAttribute {
                name: "undefined".into(),
                documents: vec![],
            },
        }];

        let result = convert_diagnostics(&diags, content);

        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].severity,
            Some(ls_types::DiagnosticSeverity::WARNING)
        );
        assert_eq!(result[0].range.start.character, 0);
        assert_eq!(result[0].range.end.character, 11);
        assert_eq!(result[0].message, "Undefined attribute: {undefined}");
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
                documents: vec![],
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
                kind: DiagnosticKind::BrokenXref {
                    target: "a".into(),
                    documents: vec![],
                },
            },
            CoreDiagnostic {
                file: PathBuf::from("/test.adoc"),
                line: 5,
                span: None,
                kind: DiagnosticKind::BrokenXref {
                    target: "b".into(),
                    documents: vec![],
                },
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

    /// An open listing in an included chapter swallows the rest of the
    /// chapter (committed fixture `tests/fixtures/unterminated_block`). The
    /// warning sits on the opener line, in the chapter only; the C file
    /// included in a closed listing has a `////` line the grammar reads as an
    /// open comment block, which must not surface. Asciidoctor:
    /// `chapter.adoc: line 5: unterminated listing block`.
    #[test]
    fn test_fixture_unterminated_block_is_reported_on_opener_line_only() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/unterminated_block")
            .canonicalize()
            .unwrap();
        let main = root.join("main.adoc");
        let chapter = root.join("chapter.adoc");
        let banner = root.join("banner.c");
        let ws = asciidoc_core::workspace::Workspace::build(&root, &main);

        let is_open =
            |d: &&CoreDiagnostic| matches!(d.kind, DiagnosticKind::UnterminatedBlock { .. });
        assert_eq!(ws.diagnostics_for(&main).iter().filter(is_open).count(), 0);

        let content = std::fs::read_to_string(&chapter).unwrap();
        let result = convert_diagnostics(ws.diagnostics_for(&chapter), &content);
        let result: Vec<_> = result
            .iter()
            .filter(|d| d.message.starts_with("Unterminated"))
            .collect();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].severity,
            Some(ls_types::DiagnosticSeverity::WARNING)
        );
        assert_eq!(
            result[0].message,
            "Unterminated listing block: no closing ----"
        );
        assert_eq!(range_text(&content, result[0].range), "----");
        assert_eq!(result[0].range.start.line, 4);

        assert!(!ws.documents()[&banner].unterminated_blocks.is_empty());
        assert_eq!(
            ws.diagnostics_for(&banner).iter().filter(is_open).count(),
            0
        );
    }

    /// A table cell holds a broken xref that wraps onto the next line, beside
    /// a one-line broken xref (committed fixture `tests/fixtures/multiline_xref`).
    /// Asciidoctor reports both; the wrapped one ends at its `>>` on the next
    /// line and does not cover the other.
    #[test]
    fn test_fixture_multiline_xref_range_ends_at_closing_brackets() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/multiline_xref")
            .canonicalize()
            .unwrap();
        let main = root.join("main.adoc");
        let content = std::fs::read_to_string(&main).unwrap();
        let ws = asciidoc_core::workspace::Workspace::build(&root, &main);

        let mut result = convert_diagnostics(ws.diagnostics_for(&main), &content);
        result.sort_by_key(|d| (d.range.start.line, d.range.start.character));

        let ranges: Vec<(u32, u32, u32, u32)> = result
            .iter()
            .map(|d| {
                let r = d.range;
                (r.start.line, r.start.character, r.end.line, r.end.character)
            })
            .collect();
        assert_eq!(ranges, vec![(3, 2, 3, 15), (3, 18, 4, 6)]);
    }
}
