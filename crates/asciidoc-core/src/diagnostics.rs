use std::collections::HashMap;
use std::path::PathBuf;

use crate::attributes::AttributeRegistry;
use crate::graph::IncludeGraph;
use crate::paths::{is_uri, resolve_doc_target};
use crate::references::ReferenceIndex;
use crate::types::{Diagnostic, DiagnosticKind, DocumentModel};

/// Collect all diagnostics across the workspace, grouped by file.
#[must_use]
pub fn collect_diagnostics(
    documents: &HashMap<PathBuf, DocumentModel>,
    graph: &IncludeGraph,
    ref_index: &ReferenceIndex,
    registry: &AttributeRegistry,
) -> HashMap<PathBuf, Vec<Diagnostic>> {
    let mut diags: HashMap<PathBuf, Vec<Diagnostic>> = HashMap::new();

    for (path, doc) in documents {
        // 1. Broken xrefs
        for xref in &doc.xrefs {
            if ref_index
                .resolve_xref(xref, path, graph, documents, registry)
                .is_err()
            {
                diags.entry(path.clone()).or_default().push(Diagnostic {
                    file: path.clone(),
                    line: xref.line,
                    span: xref.span,
                    kind: DiagnosticKind::BrokenXref {
                        target: xref.target.clone(),
                    },
                });
            }
        }

        // 2. Broken includes: check if include targets exist in graph
        for inc in &doc.includes {
            if is_uri(&registry.substitute(path, &inc.path)) {
                continue;
            }
            let Some(target_path) = resolve_doc_target(path, &inc.path, registry) else {
                continue;
            };
            if !documents.contains_key(&target_path) && !target_path.exists() {
                diags.entry(path.clone()).or_default().push(Diagnostic {
                    file: path.clone(),
                    line: inc.line,
                    span: None,
                    kind: DiagnosticKind::BrokenInclude {
                        path: inc.path.clone(),
                    },
                });
            }
        }
    }

    // 3. Duplicate anchors
    for (id, dup_loc) in &ref_index.duplicates {
        if let Some(first_loc) = ref_index.anchors.get(id) {
            diags
                .entry(dup_loc.file.clone())
                .or_default()
                .push(Diagnostic {
                    file: dup_loc.file.clone(),
                    line: dup_loc.line,
                    span: None,
                    kind: DiagnosticKind::DuplicateAnchor {
                        id: id.clone(),
                        first_file: first_loc.file.clone(),
                        first_line: first_loc.line,
                    },
                });
        }
    }

    // 4. Undefined conditional attributes (ifdef/ifndef)
    // Suppressed: ifdef/ifndef attributes are often only defined at build time,
    // producing false-positive noise. The UndefinedConditionalAttr variant is
    // retained in DiagnosticKind for a future typo-detector.

    // 5. Circular includes
    // Rank the converted document for cycles without an outside entry: a
    // level-0 title that no include shifts, best if it also sets `:doctype:`
    // (a part file of a book has a title but not the doctype).
    let rank = |path: &std::path::Path| {
        let Some(doc) = documents.get(path) else {
            return 2;
        };
        let is_header =
            doc.sections.iter().any(|s| s.level == 0) && !graph.is_level_shifted(path, documents);
        match (
            is_header,
            doc.attributes.iter().any(|a| a.name == "doctype"),
        ) {
            (true, true) => 0,
            (true, false) => 1,
            (false, _) => 2,
        }
    };
    for (source, line, target) in graph.detect_cycles(rank) {
        let path_display = target.file_name().map_or_else(
            || target.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        diags.entry(source.clone()).or_default().push(Diagnostic {
            file: source.clone(),
            line,
            span: None,
            kind: DiagnosticKind::CircularInclude {
                path: path_display.clone(),
                cycle_through: target.clone(),
            },
        });
    }

    // 6. PDF theme and font paths
    for diag in crate::pdf_theme::collect_pdf_diagnostics(documents, graph, registry) {
        diags.entry(diag.file.clone()).or_default().push(diag);
    }

    diags
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Anchor, IncludeMacro, Section, Xref};
    use pretty_assertions::assert_eq;
    use std::path::Path;

    fn make_registry() -> AttributeRegistry {
        AttributeRegistry::new(Path::new("/tmp"))
    }

    /// Flatten per-file diagnostics into a single Vec for simpler test assertions.
    fn flatten(diags: &HashMap<PathBuf, Vec<Diagnostic>>) -> Vec<&Diagnostic> {
        diags.values().flatten().collect()
    }

    #[test]
    fn test_broken_xref_diagnostic() {
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel::test("/a.adoc").with_xrefs(vec![Xref {
                target: "nonexistent".into(),
                display_text: None,
                line: 5,
                span: None,
                target_col: 0,
            }]),
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);
        let flat = flatten(&diags);

        assert_eq!(flat.len(), 1);
        assert!(matches!(
            &flat[0].kind,
            DiagnosticKind::BrokenXref { target } if target == "nonexistent"
        ));
    }

    #[test]
    fn test_broken_include_diagnostic() {
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel::test("/a.adoc").with_includes(vec![IncludeMacro {
                path: "nonexistent.adoc".into(),
                attributes: vec![],
                line: 3,
            }]),
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);

        let flat = flatten(&diags);
        assert!(flat
            .iter()
            .any(|d| matches!(&d.kind, DiagnosticKind::BrokenInclude { .. })));
    }

    #[test]
    fn test_duplicate_anchor_diagnostic_same_file() {
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel::test("/a.adoc").with_anchors(vec![
                Anchor {
                    id: "dup".into(),
                    reftext: None,
                    line: 1,
                },
                Anchor {
                    id: "dup".into(),
                    reftext: None,
                    line: 10,
                },
            ]),
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);

        let flat = flatten(&diags);
        assert!(flat
            .iter()
            .any(|d| matches!(&d.kind, DiagnosticKind::DuplicateAnchor { id, .. } if id == "dup")));
    }

    #[test]
    fn test_no_duplicate_anchor_diagnostic_cross_file() {
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel::test("/a.adoc").with_anchors(vec![Anchor {
                id: "context".into(),
                reftext: None,
                line: 1,
            }]),
        );
        documents.insert(
            PathBuf::from("/b.adoc"),
            DocumentModel::test("/b.adoc").with_anchors(vec![Anchor {
                id: "context".into(),
                reftext: None,
                line: 1,
            }]),
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);

        let flat = flatten(&diags);
        assert!(!flat
            .iter()
            .any(|d| matches!(&d.kind, DiagnosticKind::DuplicateAnchor { .. })));
    }

    #[test]
    fn test_circular_include_diagnostic() {
        let registry = make_registry();
        let a = PathBuf::from("/a.adoc");
        let b = PathBuf::from("/b.adoc");

        let mut documents = HashMap::new();
        documents.insert(
            a.clone(),
            DocumentModel::test("/a.adoc").with_includes(vec![IncludeMacro {
                path: "b.adoc".into(),
                attributes: vec![],
                line: 3,
            }]),
        );
        documents.insert(
            b.clone(),
            DocumentModel::test("/b.adoc").with_includes(vec![IncludeMacro {
                path: "a.adoc".into(),
                attributes: vec![],
                line: 5,
            }]),
        );

        let mut graph = IncludeGraph::new();
        graph.add_edge(
            &a,
            crate::types::IncludeEdge {
                target: b.clone(),
                line: 3,
                attributes: vec![],
            },
        );
        graph.add_edge(
            &b,
            crate::types::IncludeEdge {
                target: a.clone(),
                line: 5,
                attributes: vec![],
            },
        );

        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);
        let flat = flatten(&diags);

        let circular: Vec<_> = flat
            .iter()
            .filter(|d| matches!(&d.kind, DiagnosticKind::CircularInclude { .. }))
            .collect();
        assert_eq!(
            circular.len(),
            1,
            "Expected 1 CircularInclude diagnostic, got: {circular:?}"
        );
    }

    #[test]
    fn test_include_with_attribute_path_no_false_positive() {
        // Regression test: includes with {attr} paths must be substituted
        // before checking existence (was bug: raw path checked, not resolved)
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create target file on disk, below the top-level document's folder
        let sub = root.join("book/appendix");
        std::fs::create_dir_all(&sub).unwrap();
        let target_file = sub.join("glossary.adoc");
        std::fs::write(&target_file, "= Glossary\n").unwrap();

        // docdir = folder of the top-level document, not the workspace root
        let mut registry = AttributeRegistry::new(root);
        let parent = root.join("book/index.adoc");
        registry.register_file_attributes(&parent, &[]);

        // Parent doc includes {docdir}/appendix/glossary.adoc
        let mut documents = HashMap::new();
        documents.insert(
            parent.clone(),
            DocumentModel::test(parent.to_str().unwrap()).with_includes(vec![IncludeMacro {
                path: "{docdir}/appendix/glossary.adoc".into(),
                attributes: vec![],
                line: 20,
            }]),
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);
        let flat = flatten(&diags);

        let broken_includes: Vec<_> = flat
            .iter()
            .filter(|d| matches!(&d.kind, DiagnosticKind::BrokenInclude { .. }))
            .collect();
        assert!(
            broken_includes.is_empty(),
            "Include with {{docdir}} attribute should not be reported as broken, got: {:?}",
            broken_includes
        );
    }

    #[test]
    fn test_workspace_diagnostics_cover_all_files() {
        let registry = make_registry();
        let mut documents = HashMap::new();

        // Entry file (clean — no diagnostics)
        documents.insert(
            PathBuf::from("/entry.adoc"),
            DocumentModel::test("/entry.adoc")
                .with_sections(vec![Section {
                    level: 1,
                    title: "Entry".into(),
                    line: 1,
                    explicit_id: false,
                    is_header: false,
                }])
                .with_anchors(vec![Anchor {
                    id: "entry-anchor".into(),
                    reftext: None,
                    line: 3,
                }]),
        );

        // File with broken xref
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel::test("/a.adoc").with_xrefs(vec![Xref {
                target: "nonexistent-target".into(),
                display_text: None,
                line: 2,
                span: None,
                target_col: 0,
            }]),
        );

        // File with broken include
        documents.insert(
            PathBuf::from("/b.adoc"),
            DocumentModel::test("/b.adoc").with_includes(vec![IncludeMacro {
                path: "missing.adoc".into(),
                attributes: vec![],
                line: 4,
            }]),
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);

        // Diagnostics must cover multiple files — HashMap keys are the files
        assert!(
            diags.len() >= 2,
            "Expected diagnostics for at least 2 files, got {:?}",
            diags.keys().collect::<Vec<_>>()
        );
        assert!(diags.contains_key(&PathBuf::from("/a.adoc")));
        assert!(diags.contains_key(&PathBuf::from("/b.adoc")));
    }

    // --- Conditional attribute diagnostics suppressed ---
    // ifdef/ifndef attributes are not diagnosed because they are often
    // only defined at build time, producing false-positive noise.

    #[test]
    fn test_ifdef_undefined_attr_no_diagnostic() {
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel::test("/a.adoc").with_conditionals(vec![
                crate::types::ConditionalDirective {
                    kind: crate::types::ConditionalKind::Ifdef,
                    attributes: vec!["nonexistent".into()],
                    operator: crate::types::ConditionalOperator::Single,
                    line: 5,
                },
            ]),
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);
        let flat = flatten(&diags);

        let cond_diags: Vec<_> = flat
            .iter()
            .filter(|d| matches!(&d.kind, DiagnosticKind::UndefinedConditionalAttr { .. }))
            .collect();
        assert!(
            cond_diags.is_empty(),
            "ifdef with undefined attr should not produce diagnostic, got: {cond_diags:?}"
        );
    }

    #[test]
    fn test_ifndef_undefined_attr_no_diagnostic() {
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel::test("/a.adoc").with_conditionals(vec![
                crate::types::ConditionalDirective {
                    kind: crate::types::ConditionalKind::Ifndef,
                    attributes: vec!["unknown-attr".into()],
                    operator: crate::types::ConditionalOperator::Single,
                    line: 10,
                },
            ]),
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);
        let flat = flatten(&diags);

        let cond_diags: Vec<_> = flat
            .iter()
            .filter(|d| matches!(&d.kind, DiagnosticKind::UndefinedConditionalAttr { .. }))
            .collect();
        assert!(
            cond_diags.is_empty(),
            "ifndef with undefined attr should not produce diagnostic, got: {cond_diags:?}"
        );
    }

    #[test]
    fn test_env_asciidoc_lsp_is_builtin() {
        let registry = make_registry();
        let file = Path::new("/tmp/any.adoc");
        assert!(
            registry.resolve(file, "env-asciidoc-lsp").is_some(),
            "env-asciidoc-lsp should be a builtin attribute"
        );
    }

    #[test]
    fn test_no_false_positives() {
        let registry = make_registry();
        let mut documents = HashMap::new();

        // File with valid anchor
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel::test("/a.adoc")
                .with_sections(vec![Section {
                    level: 1,
                    title: "Valid".into(),
                    line: 1,
                    explicit_id: false,
                    is_header: false,
                }])
                .with_anchors(vec![Anchor {
                    id: "valid-anchor".into(),
                    reftext: None,
                    line: 5,
                }]),
        );
        // File with xref to that anchor
        documents.insert(
            PathBuf::from("/b.adoc"),
            DocumentModel::test("/b.adoc").with_xrefs(vec![Xref {
                target: "valid-anchor".into(),
                display_text: None,
                line: 2,
                span: None,
                target_col: 0,
            }]),
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);
        let flat = flatten(&diags);

        assert!(flat.is_empty(), "Expected no diagnostics, got: {:?}", flat);
    }
}
