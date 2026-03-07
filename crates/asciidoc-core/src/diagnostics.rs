use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::attributes::AttributeRegistry;
use crate::graph::IncludeGraph;
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
                .resolve_xref(xref, path, graph, documents)
                .is_err()
            {
                diags.entry(path.clone()).or_default().push(Diagnostic {
                    file: path.clone(),
                    line: xref.line,
                    kind: DiagnosticKind::BrokenXref {
                        target: xref.target.clone(),
                    },
                    message: format!("Broken cross-reference: {}", xref.target),
                });
            }
        }

        // 2. Broken includes: check if include targets exist in graph
        for inc in &doc.includes {
            let resolved_path = registry.substitute(path, &inc.path);
            let base_dir = path.parent().unwrap_or(Path::new("."));
            let target_path = base_dir.join(&resolved_path);
            let target_canonical = target_path.canonicalize().unwrap_or(target_path.clone());
            if !documents.contains_key(&target_canonical) && !target_path.exists() {
                diags.entry(path.clone()).or_default().push(Diagnostic {
                    file: path.clone(),
                    line: inc.line,
                    kind: DiagnosticKind::BrokenInclude {
                        path: inc.path.clone(),
                    },
                    message: format!("Broken include: {}", inc.path),
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
                    kind: DiagnosticKind::DuplicateAnchor {
                        id: id.clone(),
                        first_file: first_loc.file.clone(),
                    },
                    message: format!(
                        "Duplicate anchor '{}', first defined in {}:{}",
                        id,
                        first_loc.file.display(),
                        first_loc.line
                    ),
                });
        }
    }

    // 4. Undefined conditional attributes (ifdef/ifndef)
    for (path, doc) in documents {
        for cond in &doc.conditionals {
            let directive = match cond.kind {
                crate::types::ConditionalKind::Ifdef => "ifdef",
                crate::types::ConditionalKind::Ifndef => "ifndef",
            };
            let attr_expr = cond.attributes.join(match cond.operator {
                crate::types::ConditionalOperator::And => "+",
                _ => ",",
            });

            match cond.operator {
                crate::types::ConditionalOperator::Single
                | crate::types::ConditionalOperator::And => {
                    for attr in &cond.attributes {
                        if attr.starts_with("env-") {
                            continue;
                        }
                        if registry.resolve(path, attr).is_none() {
                            diags.entry(path.clone()).or_default().push(Diagnostic {
                                file: path.clone(),
                                line: cond.line,
                                kind: DiagnosticKind::UndefinedConditionalAttr {
                                    name: attr.clone(),
                                    directive: directive.to_string(),
                                },
                                message: format!(
                                    "{directive}::{attr_expr} — attribute '{attr}' is undefined"
                                ),
                            });
                        }
                    }
                }
                crate::types::ConditionalOperator::Or => {
                    let non_env_attrs: Vec<_> = cond
                        .attributes
                        .iter()
                        .filter(|attr| !attr.starts_with("env-"))
                        .collect();

                    if non_env_attrs.is_empty() {
                        continue;
                    }

                    let any_defined = non_env_attrs
                        .iter()
                        .any(|attr| registry.resolve(path, attr).is_some());
                    if !any_defined {
                        let names = non_env_attrs
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        diags.entry(path.clone()).or_default().push(Diagnostic {
                            file: path.clone(),
                            line: cond.line,
                            kind: DiagnosticKind::UndefinedConditionalAttr {
                                name: names.clone(),
                                directive: directive.to_string(),
                            },
                            message: format!(
                                "{directive}::{attr_expr} — none of [{names}] are defined"
                            ),
                        });
                    }
                }
            }
        }
    }

    // 5. Circular includes
    for (source, line, target) in graph.detect_cycles() {
        let path_display = target.file_name().map_or_else(
            || target.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        diags.entry(source.clone()).or_default().push(Diagnostic {
            file: source.clone(),
            line,
            kind: DiagnosticKind::CircularInclude {
                path: path_display.clone(),
                cycle_through: target.clone(),
            },
            message: format!("Circular include: {path_display} creates a cycle"),
        });
    }

    diags
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Anchor, IncludeMacro, Section, Xref};

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
            DocumentModel {
                path: PathBuf::from("/a.adoc"),
                sections: vec![],
                includes: vec![],
                xrefs: vec![Xref {
                    target: "nonexistent".into(),
                    display_text: None,
                    line: 5,
                }],
                anchors: vec![],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![],
            },
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
            DocumentModel {
                path: PathBuf::from("/a.adoc"),
                sections: vec![],
                includes: vec![IncludeMacro {
                    path: "nonexistent.adoc".into(),
                    attributes: vec![],
                    line: 3,
                }],
                xrefs: vec![],
                anchors: vec![],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![],
            },
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
            DocumentModel {
                path: PathBuf::from("/a.adoc"),
                sections: vec![],
                includes: vec![],
                xrefs: vec![],
                anchors: vec![
                    Anchor {
                        id: "dup".into(),
                        line: 1,
                    },
                    Anchor {
                        id: "dup".into(),
                        line: 10,
                    },
                ],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![],
            },
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
            DocumentModel {
                path: PathBuf::from("/a.adoc"),
                sections: vec![],
                includes: vec![],
                xrefs: vec![],
                anchors: vec![Anchor {
                    id: "context".into(),
                    line: 1,
                }],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![],
            },
        );
        documents.insert(
            PathBuf::from("/b.adoc"),
            DocumentModel {
                path: PathBuf::from("/b.adoc"),
                sections: vec![],
                includes: vec![],
                xrefs: vec![],
                anchors: vec![Anchor {
                    id: "context".into(),
                    line: 1,
                }],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![],
            },
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
            DocumentModel {
                path: a.clone(),
                sections: vec![],
                includes: vec![IncludeMacro {
                    path: "b.adoc".into(),
                    attributes: vec![],
                    line: 3,
                }],
                xrefs: vec![],
                anchors: vec![],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![],
            },
        );
        documents.insert(
            b.clone(),
            DocumentModel {
                path: b.clone(),
                sections: vec![],
                includes: vec![IncludeMacro {
                    path: "a.adoc".into(),
                    attributes: vec![],
                    line: 5,
                }],
                xrefs: vec![],
                anchors: vec![],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![],
            },
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

        // Create target file on disk
        let sub = root.join("appendix");
        std::fs::create_dir_all(&sub).unwrap();
        let target_file = sub.join("glossary.adoc");
        std::fs::write(&target_file, "= Glossary\n").unwrap();

        // Registry with docdir = root
        let registry = AttributeRegistry::new(root);

        // Parent doc includes {docdir}/appendix/glossary.adoc
        let parent = root.join("index.adoc");
        let mut documents = HashMap::new();
        documents.insert(
            parent.clone(),
            DocumentModel {
                path: parent.clone(),
                sections: vec![],
                includes: vec![IncludeMacro {
                    path: "{docdir}/appendix/glossary.adoc".into(),
                    attributes: vec![],
                    line: 20,
                }],
                xrefs: vec![],
                anchors: vec![],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![],
            },
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
            DocumentModel {
                path: PathBuf::from("/entry.adoc"),
                sections: vec![Section {
                    level: 1,
                    title: "Entry".into(),
                    line: 1,
                }],
                includes: vec![],
                xrefs: vec![],
                anchors: vec![Anchor {
                    id: "entry-anchor".into(),
                    line: 3,
                }],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![],
            },
        );

        // File with broken xref
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel {
                path: PathBuf::from("/a.adoc"),
                sections: vec![],
                includes: vec![],
                xrefs: vec![Xref {
                    target: "nonexistent-target".into(),
                    display_text: None,
                    line: 2,
                }],
                anchors: vec![],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![],
            },
        );

        // File with broken include
        documents.insert(
            PathBuf::from("/b.adoc"),
            DocumentModel {
                path: PathBuf::from("/b.adoc"),
                sections: vec![],
                includes: vec![IncludeMacro {
                    path: "missing.adoc".into(),
                    attributes: vec![],
                    line: 4,
                }],
                xrefs: vec![],
                anchors: vec![],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![],
            },
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

    // --- Conditional attribute diagnostics (adoc-0062) ---

    #[test]
    fn test_ifdef_undefined_attr_produces_diagnostic() {
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel {
                path: PathBuf::from("/a.adoc"),
                sections: vec![],
                includes: vec![],
                xrefs: vec![],
                anchors: vec![],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![crate::types::ConditionalDirective {
                    kind: crate::types::ConditionalKind::Ifdef,
                    attributes: vec!["nonexistent".into()],
                    operator: crate::types::ConditionalOperator::Single,
                    line: 5,
                }],
            },
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);
        let flat = flatten(&diags);

        let cond_diags: Vec<_> = flat
            .iter()
            .filter(|d| matches!(&d.kind, DiagnosticKind::UndefinedConditionalAttr { .. }))
            .collect();
        assert_eq!(
            cond_diags.len(),
            1,
            "Expected 1 conditional diagnostic, got: {cond_diags:?}"
        );
        assert_eq!(cond_diags[0].line, 5);
    }

    #[test]
    fn test_ifdef_defined_attr_no_diagnostic() {
        let tmp = tempfile::tempdir().unwrap();
        let mut registry = AttributeRegistry::new(tmp.path());
        // Register an attribute in the registry
        let file = PathBuf::from("/a.adoc");
        registry.register_file_attributes(
            &file,
            &[crate::types::AttributeDef {
                name: "backend".into(),
                value: "html5".into(),
                line: 2,
            }],
        );

        let mut documents = HashMap::new();
        documents.insert(
            file.clone(),
            DocumentModel {
                path: file.clone(),
                sections: vec![],
                includes: vec![],
                xrefs: vec![],
                anchors: vec![],
                attributes: vec![crate::types::AttributeDef {
                    name: "backend".into(),
                    value: "html5".into(),
                    line: 2,
                }],
                attribute_refs: vec![],
                conditionals: vec![crate::types::ConditionalDirective {
                    kind: crate::types::ConditionalKind::Ifdef,
                    attributes: vec!["backend".into()],
                    operator: crate::types::ConditionalOperator::Single,
                    line: 5,
                }],
            },
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
            "Defined attr should not produce diagnostic, got: {cond_diags:?}"
        );
    }

    #[test]
    fn test_ifndef_undefined_attr_produces_diagnostic() {
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel {
                path: PathBuf::from("/a.adoc"),
                sections: vec![],
                includes: vec![],
                xrefs: vec![],
                anchors: vec![],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![crate::types::ConditionalDirective {
                    kind: crate::types::ConditionalKind::Ifndef,
                    attributes: vec!["unknown-attr".into()],
                    operator: crate::types::ConditionalOperator::Single,
                    line: 10,
                }],
            },
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);
        let flat = flatten(&diags);

        let cond_diags: Vec<_> = flat
            .iter()
            .filter(|d| matches!(&d.kind, DiagnosticKind::UndefinedConditionalAttr { .. }))
            .collect();
        assert_eq!(
            cond_diags.len(),
            1,
            "Expected 1 conditional diagnostic, got: {cond_diags:?}"
        );
    }

    #[test]
    fn test_ifdef_and_reports_each_undefined() {
        // ifdef::a+b[] with both undefined → 2 diagnostics
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel {
                path: PathBuf::from("/a.adoc"),
                sections: vec![],
                includes: vec![],
                xrefs: vec![],
                anchors: vec![],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![crate::types::ConditionalDirective {
                    kind: crate::types::ConditionalKind::Ifdef,
                    attributes: vec!["backend".into(), "icons".into()],
                    operator: crate::types::ConditionalOperator::And,
                    line: 3,
                }],
            },
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);
        let flat = flatten(&diags);

        let cond_diags: Vec<_> = flat
            .iter()
            .filter(|d| matches!(&d.kind, DiagnosticKind::UndefinedConditionalAttr { .. }))
            .collect();
        assert_eq!(
            cond_diags.len(),
            2,
            "AND with 2 undefined attrs should produce 2 diagnostics, got: {cond_diags:?}"
        );
    }

    #[test]
    fn test_ifdef_or_no_diagnostic_when_one_defined() {
        // ifdef::backend,icons[] with backend defined → no diagnostic
        let tmp = tempfile::tempdir().unwrap();
        let mut registry = AttributeRegistry::new(tmp.path());
        let file = PathBuf::from("/a.adoc");
        registry.register_file_attributes(
            &file,
            &[crate::types::AttributeDef {
                name: "backend".into(),
                value: "html5".into(),
                line: 2,
            }],
        );

        let mut documents = HashMap::new();
        documents.insert(
            file.clone(),
            DocumentModel {
                path: file.clone(),
                sections: vec![],
                includes: vec![],
                xrefs: vec![],
                anchors: vec![],
                attributes: vec![crate::types::AttributeDef {
                    name: "backend".into(),
                    value: "html5".into(),
                    line: 2,
                }],
                attribute_refs: vec![],
                conditionals: vec![crate::types::ConditionalDirective {
                    kind: crate::types::ConditionalKind::Ifdef,
                    attributes: vec!["backend".into(), "icons".into()],
                    operator: crate::types::ConditionalOperator::Or,
                    line: 5,
                }],
            },
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
            "OR with one defined attr should produce no diagnostic, got: {cond_diags:?}"
        );
    }

    #[test]
    fn test_ifdef_or_diagnostic_when_none_defined() {
        // ifdef::backend,icons[] with none defined → 1 diagnostic
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel {
                path: PathBuf::from("/a.adoc"),
                sections: vec![],
                includes: vec![],
                xrefs: vec![],
                anchors: vec![],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![crate::types::ConditionalDirective {
                    kind: crate::types::ConditionalKind::Ifdef,
                    attributes: vec!["backend".into(), "icons".into()],
                    operator: crate::types::ConditionalOperator::Or,
                    line: 3,
                }],
            },
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);
        let flat = flatten(&diags);

        let cond_diags: Vec<_> = flat
            .iter()
            .filter(|d| matches!(&d.kind, DiagnosticKind::UndefinedConditionalAttr { .. }))
            .collect();
        assert_eq!(
            cond_diags.len(),
            1,
            "OR with none defined should produce 1 diagnostic, got: {cond_diags:?}"
        );
        assert!(cond_diags[0].message.contains("none of"));
    }

    // --- env-* attribute suppression (adoc-0070) ---

    #[test]
    fn test_ifdef_env_vscode_no_diagnostic() {
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel {
                path: PathBuf::from("/a.adoc"),
                sections: vec![],
                includes: vec![],
                xrefs: vec![],
                anchors: vec![],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![crate::types::ConditionalDirective {
                    kind: crate::types::ConditionalKind::Ifdef,
                    attributes: vec!["env-vscode".into()],
                    operator: crate::types::ConditionalOperator::Single,
                    line: 1,
                }],
            },
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
            "ifdef::env-vscode[] should not produce diagnostic, got: {cond_diags:?}"
        );
    }

    #[test]
    fn test_ifndef_env_idea_no_diagnostic() {
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel {
                path: PathBuf::from("/a.adoc"),
                sections: vec![],
                includes: vec![],
                xrefs: vec![],
                anchors: vec![],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![crate::types::ConditionalDirective {
                    kind: crate::types::ConditionalKind::Ifndef,
                    attributes: vec!["env-idea".into()],
                    operator: crate::types::ConditionalOperator::Single,
                    line: 2,
                }],
            },
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
            "ifndef::env-idea[] should not produce diagnostic, got: {cond_diags:?}"
        );
    }

    #[test]
    fn test_ifdef_env_unknown_no_diagnostic() {
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel {
                path: PathBuf::from("/a.adoc"),
                sections: vec![],
                includes: vec![],
                xrefs: vec![],
                anchors: vec![],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![crate::types::ConditionalDirective {
                    kind: crate::types::ConditionalKind::Ifdef,
                    attributes: vec!["env-whatever-unknown".into()],
                    operator: crate::types::ConditionalOperator::Single,
                    line: 3,
                }],
            },
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
            "ifdef::env-whatever-unknown[] should not produce diagnostic, got: {cond_diags:?}"
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
    fn test_ifdef_non_env_still_warns() {
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel {
                path: PathBuf::from("/a.adoc"),
                sections: vec![],
                includes: vec![],
                xrefs: vec![],
                anchors: vec![],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![crate::types::ConditionalDirective {
                    kind: crate::types::ConditionalKind::Ifdef,
                    attributes: vec!["wirklich-undefined".into()],
                    operator: crate::types::ConditionalOperator::Single,
                    line: 5,
                }],
            },
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);
        let flat = flatten(&diags);

        let cond_diags: Vec<_> = flat
            .iter()
            .filter(|d| matches!(&d.kind, DiagnosticKind::UndefinedConditionalAttr { .. }))
            .collect();
        assert_eq!(
            cond_diags.len(),
            1,
            "Non-env undefined attr should still warn, got: {cond_diags:?}"
        );
    }

    #[test]
    fn test_ifdef_or_env_mixed() {
        // ifdef::env-vscode,undefined-attr[] → warning only for undefined-attr
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel {
                path: PathBuf::from("/a.adoc"),
                sections: vec![],
                includes: vec![],
                xrefs: vec![],
                anchors: vec![],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![crate::types::ConditionalDirective {
                    kind: crate::types::ConditionalKind::Ifdef,
                    attributes: vec!["env-vscode".into(), "undefined-attr".into()],
                    operator: crate::types::ConditionalOperator::Or,
                    line: 7,
                }],
            },
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);
        let flat = flatten(&diags);

        let cond_diags: Vec<_> = flat
            .iter()
            .filter(|d| matches!(&d.kind, DiagnosticKind::UndefinedConditionalAttr { .. }))
            .collect();
        assert_eq!(
            cond_diags.len(),
            1,
            "OR with env-vscode + undefined-attr should warn for undefined-attr only, got: {cond_diags:?}"
        );
        assert!(
            cond_diags[0].message.contains("undefined-attr"),
            "Warning should mention undefined-attr, got: {}",
            cond_diags[0].message
        );
    }

    #[test]
    fn test_no_false_positives() {
        let registry = make_registry();
        let mut documents = HashMap::new();

        // File with valid anchor
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel {
                path: PathBuf::from("/a.adoc"),
                sections: vec![Section {
                    level: 1,
                    title: "Valid".into(),
                    line: 1,
                }],
                includes: vec![],
                xrefs: vec![],
                anchors: vec![Anchor {
                    id: "valid-anchor".into(),
                    line: 5,
                }],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![],
            },
        );
        // File with xref to that anchor
        documents.insert(
            PathBuf::from("/b.adoc"),
            DocumentModel {
                path: PathBuf::from("/b.adoc"),
                sections: vec![],
                includes: vec![],
                xrefs: vec![Xref {
                    target: "valid-anchor".into(),
                    display_text: None,
                    line: 2,
                }],
                anchors: vec![],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![],
            },
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);
        let flat = flatten(&diags);

        assert!(flat.is_empty(), "Expected no diagnostics, got: {:?}", flat);
    }
}
