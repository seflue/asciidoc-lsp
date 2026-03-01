use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::attributes::AttributeRegistry;
use crate::graph::IncludeGraph;
use crate::references::ReferenceIndex;
use crate::types::{Diagnostic, DiagnosticKind, DocumentModel};

/// Collect all diagnostics across the workspace.
#[must_use]
pub fn collect_diagnostics(
    documents: &HashMap<PathBuf, DocumentModel>,
    graph: &IncludeGraph,
    ref_index: &ReferenceIndex,
    registry: &AttributeRegistry,
) -> Vec<Diagnostic> {
    let mut diags = Vec::new();

    for (path, doc) in documents {
        // 1. Broken xrefs
        for xref in &doc.xrefs {
            if ref_index
                .resolve_xref(xref, path, graph, documents)
                .is_err()
            {
                diags.push(Diagnostic {
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
                diags.push(Diagnostic {
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
            diags.push(Diagnostic {
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

    diags
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::types::{Anchor, IncludeMacro, Section, Xref};

    fn make_registry() -> AttributeRegistry {
        AttributeRegistry::new(Path::new("/tmp"))
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
            },
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);

        assert_eq!(diags.len(), 1);
        assert!(matches!(
            &diags[0].kind,
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
            },
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);

        assert!(diags
            .iter()
            .any(|d| matches!(&d.kind, DiagnosticKind::BrokenInclude { .. })));
    }

    #[test]
    fn test_duplicate_anchor_diagnostic() {
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
                    id: "dup".into(),
                    line: 1,
                }],
                attributes: vec![],
                attribute_refs: vec![],
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
                    id: "dup".into(),
                    line: 10,
                }],
                attributes: vec![],
                attribute_refs: vec![],
            },
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);

        assert!(diags
            .iter()
            .any(|d| matches!(&d.kind, DiagnosticKind::DuplicateAnchor { id, .. } if id == "dup")));
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
            },
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);

        let broken_includes: Vec<_> = diags
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
            },
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);

        // Diagnostics must cover multiple files, not just one
        let files_with_diags: HashSet<_> = diags.iter().map(|d| &d.file).collect();
        assert!(
            files_with_diags.len() >= 2,
            "Expected diagnostics for at least 2 files, got {:?}",
            files_with_diags
        );
        assert!(files_with_diags.contains(&PathBuf::from("/a.adoc")));
        assert!(files_with_diags.contains(&PathBuf::from("/b.adoc")));
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
            },
        );

        let graph = IncludeGraph::new();
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diags = collect_diagnostics(&documents, &graph, &ref_index, &registry);

        assert!(
            diags.is_empty(),
            "Expected no diagnostics, got: {:?}",
            diags
        );
    }
}
