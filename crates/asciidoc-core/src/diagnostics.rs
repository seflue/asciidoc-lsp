use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::attributes::AttributeRegistry;
use crate::graph::IncludeGraph;
use crate::paths::{is_uri, resolve_doc_target};
use crate::references::ReferenceIndex;
use crate::types::{Diagnostic, DiagnosticKind, DocumentModel};

/// Collect all diagnostics across the workspace, grouped by file.
///
/// Paths named inside messages are display paths, see `display_path`:
/// relative to `root` where that is short, absolute otherwise.
#[must_use]
pub fn collect_diagnostics(
    root: &Path,
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
                        first_file: display_path(root, &first_loc.file),
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
                cycle_through: display_path(root, &target),
            },
        });
    }

    // 6. PDF theme and font paths
    for mut diag in crate::pdf_theme::collect_pdf_diagnostics(documents, graph, registry) {
        match &mut diag.kind {
            DiagnosticKind::PdfPathNotFound { path, .. } => *path = pdf_display_path(root, path),
            DiagnosticKind::PdfThemeYamlExtension { value } => {
                *value = pdf_display_path(root, Path::new(value.as_str()))
                    .display()
                    .to_string();
            }
            _ => {}
        }
        diags.entry(diag.file.clone()).or_default().push(diag);
    }

    diags
}

/// Most `..` steps a displayed relative path may need: covers a sibling
/// folder (`../shared/x.adoc`) but not an unrecognisable long chain.
const MAX_PARENT_STEPS: usize = 2;

/// A file the scan reached (`DuplicateAnchor`, `CircularInclude`) as shown in
/// a message. A relative path stays as it is. An absolute path is
/// normalized lexically and shown relative to the workspace root when it is
/// under the root or shares a folder with it and needs at most
/// `MAX_PARENT_STEPS` `..` steps. Otherwise (outside the root, like
/// `/opt/themes`) it stays absolute, independent of how long the shared
/// ancestor is.
fn display_path(root: &Path, path: &Path) -> PathBuf {
    if !path.is_absolute() {
        return path.to_path_buf();
    }
    let path = crate::paths::normalize_lexically(path);
    let rel = crate::paths::relative_path(root, &path);
    let parent_steps = rel
        .components()
        .take_while(|c| matches!(c, std::path::Component::ParentDir))
        .count();
    let shares_a_folder = root
        .components()
        .zip(path.components())
        .take_while(|(a, b)| a == b)
        .any(|(a, _)| matches!(a, std::path::Component::Normal(_)));
    if shares_a_folder && parent_steps <= MAX_PARENT_STEPS {
        rel
    } else {
        path
    }
}

/// A PDF value as shown in a message: root-relative only when it lies under
/// `root`, absolute otherwise, so a value the user wrote outside the root is
/// never rewritten into a `..` form. A relative value stays as written.
///
/// Known gap: `root` is the canonical root. A document inside a
/// non-canonical root that is a symlink to a target outside it has its
/// `{docdir}` in the non-canonical form, so a path derived from it is not
/// recognised as under the root and stays absolute.
fn pdf_display_path(root: &Path, path: &Path) -> PathBuf {
    if !path.is_absolute() {
        return path.to_path_buf();
    }
    let path = crate::paths::normalize_lexically(path);
    match path.strip_prefix(root) {
        Ok(rel) => rel.to_path_buf(),
        Err(_) => path,
    }
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
        let diags = collect_diagnostics(Path::new("/"), &documents, &graph, &ref_index, &registry);
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
        let diags = collect_diagnostics(Path::new("/"), &documents, &graph, &ref_index, &registry);

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
        let diags = collect_diagnostics(Path::new("/"), &documents, &graph, &ref_index, &registry);

        let flat = flatten(&diags);
        assert!(flat
            .iter()
            .any(|d| matches!(&d.kind, DiagnosticKind::DuplicateAnchor { id, .. } if id == "dup")));
    }

    /// A file sharing only `/` with the root keeps its absolute path: a
    /// `../../..` chain would be longer and unrecognisable.
    #[test]
    fn test_duplicate_anchor_far_outside_root_keeps_absolute_path() {
        let registry = make_registry();
        let far = "/other-tree/note.adoc";
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from(far),
            DocumentModel::test(far).with_anchors(vec![
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
        let diags = collect_diagnostics(
            Path::new("/work/deep/root"),
            &documents,
            &graph,
            &ref_index,
            &registry,
        );
        let messages: Vec<String> = flatten(&diags).iter().map(|d| d.kind.to_string()).collect();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("first defined in /other-tree/note.adoc:1")),
            "got: {messages:?}"
        );
    }

    #[test]
    fn test_display_path_rules() {
        let root = Path::new("/work/proj");
        assert_eq!(
            display_path(root, Path::new("/work/proj/a/b.adoc")),
            PathBuf::from("a/b.adoc")
        );
        assert_eq!(
            display_path(root, Path::new("/work/shared/x.adoc")),
            PathBuf::from("../shared/x.adoc")
        );
        assert_eq!(
            display_path(root, Path::new("/opt/themes")),
            PathBuf::from("/opt/themes")
        );
        assert_eq!(
            display_path(root, Path::new("rel/x")),
            PathBuf::from("rel/x")
        );
        // Shares a real ancestor with a deep root: a long `../` chain is
        // unrecognisable, so the absolute path stays, however long the
        // shared prefix makes it.
        assert_eq!(
            display_path(Path::new("/a/b/c/d/e/proj"), Path::new("/a/other.adoc")),
            PathBuf::from("/a/other.adoc")
        );
        assert_eq!(
            display_path(
                Path::new("/very/long/shared/ancestor/path/d1/d2/d3/d4"),
                Path::new("/very/long/shared/ancestor/path/x.adoc")
            ),
            PathBuf::from("/very/long/shared/ancestor/path/x.adoc")
        );
        // Three steps up exceeds MAX_PARENT_STEPS.
        assert_eq!(
            display_path(
                Path::new("/work/proj/sub/deeper"),
                Path::new("/work/x/y.adoc")
            ),
            PathBuf::from("/work/x/y.adoc")
        );
        // Two steps up is still shown relative.
        assert_eq!(
            display_path(Path::new("/work/proj/sub"), Path::new("/work/x/y.adoc")),
            PathBuf::from("../../x/y.adoc")
        );
    }

    #[test]
    fn test_display_path_normalizes_inner_parent_segments() {
        assert_eq!(
            display_path(
                Path::new("/work/proj"),
                Path::new("/work/proj/a/../theme/x.yaml")
            ),
            PathBuf::from("theme/x.yaml")
        );
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
        let diags = collect_diagnostics(Path::new("/"), &documents, &graph, &ref_index, &registry);

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
        let diags = collect_diagnostics(Path::new("/"), &documents, &graph, &ref_index, &registry);
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
        let diags = collect_diagnostics(Path::new("/"), &documents, &graph, &ref_index, &registry);
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
        let diags = collect_diagnostics(Path::new("/"), &documents, &graph, &ref_index, &registry);

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
        let diags = collect_diagnostics(Path::new("/"), &documents, &graph, &ref_index, &registry);
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
        let diags = collect_diagnostics(Path::new("/"), &documents, &graph, &ref_index, &registry);
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
        let diags = collect_diagnostics(Path::new("/"), &documents, &graph, &ref_index, &registry);
        let flat = flatten(&diags);

        assert!(flat.is_empty(), "Expected no diagnostics, got: {:?}", flat);
    }
}
