use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::anchor_scope::{AnchorScope, CatalogCache};
use crate::paths::{is_uri, resolve_doc_target};
use crate::types::{Diagnostic, DiagnosticKind, Span};
use crate::workspace::Workspace;

/// A duplicate: the second definition (file, line), the ID, and the first.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct DuplicateKey<'a> {
    file: &'a Path,
    line: usize,
    id: &'a str,
    first_file: &'a Path,
    first_line: usize,
}

/// Collect all diagnostics across the workspace, grouped by file.
///
/// Paths named inside messages are display paths, see `display_path`:
/// relative to `root` where that is short, absolute otherwise.
#[must_use]
pub fn collect_diagnostics(
    root: &Path,
    workspace: &Workspace,
) -> HashMap<PathBuf, Vec<Diagnostic>> {
    let documents = &workspace.documents;
    let graph = &workspace.include_graph;
    let registry = &workspace.attributes;
    // One catalog per top-level document, read once per recalculation; a
    // file outside every top-level document's reach is added on first use.
    let catalogs = CatalogCache::new(workspace);
    let top_level: Vec<Rc<AnchorScope>> = catalogs.all();
    let mut diags: HashMap<PathBuf, Vec<Diagnostic>> = HashMap::new();
    // The references each top-level document lacks the attribute for, walked
    // once per recalculation.
    let mut undefined_refs: HashMap<PathBuf, HashSet<(PathBuf, usize, usize)>> = HashMap::new();

    for (path, doc) in documents {
        // 1. Broken xrefs, checked in each top-level document that reads the
        // file (the same ones go-to-definition uses): one diagnostic, naming
        // the documents where the target is missing when several read it.
        let reading: Vec<Rc<AnchorScope>> =
            if doc.xrefs.is_empty() && doc.substituted_refs.is_empty() {
                Vec::new()
            } else {
                catalogs.scopes_at(path)
            };
        for xref in &doc.xrefs {
            // The file part of the target is resolved in each reading
            // document's own attribute scope, as go-to-definition does; the
            // scopes are only needed when the target has an attribute.
            let attribute_scopes = if xref.target.contains('{') {
                workspace.scopes_at(path, xref.line)
            } else {
                Vec::new()
            };
            // A document that reads the file only in part is no judge of what
            // is missing in it, so it counts neither as missing nor as found.
            let missing: Vec<PathBuf> = reading
                .iter()
                .filter(|scope| scope.reads_whole(path))
                .filter(|scope| {
                    catalogs
                        .resolve_read(scope, path, xref, &attribute_scopes)
                        .is_err()
                })
                .map(|scope| display_path(root, scope.document()))
                .collect();
            if !missing.is_empty() {
                diags.entry(path.clone()).or_default().push(Diagnostic {
                    file: path.clone(),
                    line: xref.line,
                    span: xref.span,
                    kind: DiagnosticKind::BrokenXref {
                        target: xref.target.clone(),
                        documents: if reading.len() > 1 {
                            missing
                        } else {
                            Vec::new()
                        },
                    },
                });
            }
        }

        // 1b. Undefined attributes, per top-level document like the xrefs:
        // one diagnostic per reference, naming the documents that lack the
        // attribute when several read the file.
        for reference in &doc.substituted_refs {
            let position = (path.clone(), reference.line, reference.start_col);
            let missing: Vec<PathBuf> = reading
                .iter()
                .filter(|scope| scope.reads_whole(path))
                .filter(|scope| {
                    undefined_refs
                        .entry(scope.document().to_path_buf())
                        .or_insert_with(|| workspace.undefined_attribute_refs(scope.document()))
                        .contains(&position)
                })
                .map(|scope| display_path(root, scope.document()))
                .collect();
            if !missing.is_empty() {
                diags.entry(path.clone()).or_default().push(Diagnostic {
                    file: path.clone(),
                    line: reference.line,
                    span: Some(Span {
                        start_col: reference.start_col,
                        end_line: reference.line,
                        end_col: reference.end_col,
                    }),
                    kind: DiagnosticKind::UndefinedAttribute {
                        name: reference.name.clone(),
                        documents: if reading.len() > 1 {
                            missing
                        } else {
                            Vec::new()
                        },
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

    // 3. Duplicate anchors, per top-level document: the second definition in
    // reading order. Documents that read the same pair share one diagnostic.
    let mut found: BTreeMap<DuplicateKey<'_>, Vec<PathBuf>> = BTreeMap::new();
    for scope in &top_level {
        for duplicate in scope.duplicates() {
            let key = DuplicateKey {
                file: &duplicate.second.file,
                line: duplicate.second.line,
                id: &duplicate.id,
                first_file: &duplicate.first.file,
                first_line: duplicate.first.line,
            };
            let readers = found.entry(key).or_default();
            // A document that reads the pair more than once is named once.
            if !readers.iter().any(|d| d == scope.document()) {
                readers.push(scope.document().to_path_buf());
            }
        }
    }
    for (key, mut documents) in found {
        documents.sort();
        diags
            .entry(key.file.to_path_buf())
            .or_default()
            .push(Diagnostic {
                file: key.file.to_path_buf(),
                line: key.line,
                span: None,
                kind: DiagnosticKind::DuplicateAnchor {
                    id: key.id.to_string(),
                    first_file: display_path(root, key.first_file),
                    first_line: key.first_line,
                    documents: documents
                        .iter()
                        .map(|document| display_path(root, document))
                        .collect(),
                },
            });
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
    use crate::attributes::AttributeRegistry;
    use crate::graph::IncludeGraph;
    use crate::types::{
        Anchor, AttributeDef, AttributeRef, DocumentModel, IncludeMacro, Section, Xref,
    };
    use pretty_assertions::assert_eq;
    use std::path::Path;

    fn make_registry() -> AttributeRegistry {
        AttributeRegistry::new(Path::new("/tmp"))
    }

    /// The diagnostics of a workspace made of `documents`, as `Workspace::build`
    /// would index them.
    fn diagnose(
        documents: HashMap<PathBuf, DocumentModel>,
        graph: IncludeGraph,
        registry: AttributeRegistry,
    ) -> HashMap<PathBuf, Vec<Diagnostic>> {
        diagnose_in(Path::new("/"), documents, graph, registry)
    }

    /// As `diagnose`, with messages shown relative to `root`.
    fn diagnose_in(
        root: &Path,
        documents: HashMap<PathBuf, DocumentModel>,
        graph: IncludeGraph,
        registry: AttributeRegistry,
    ) -> HashMap<PathBuf, Vec<Diagnostic>> {
        let workspace = Workspace::new_with(
            PathBuf::from("/tmp"),
            registry,
            graph,
            documents,
            HashMap::new(),
        );
        collect_diagnostics(root, &workspace)
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
        let diags = diagnose(documents, graph, registry);
        let flat = flatten(&diags);

        assert_eq!(flat.len(), 1);
        assert!(matches!(
            &flat[0].kind,
            DiagnosticKind::BrokenXref { target, .. } if target == "nonexistent"
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
                target_col: 9,
            }]),
        );

        let graph = IncludeGraph::new();
        let diags = diagnose(documents, graph, registry);

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
        let diags = diagnose(documents, graph, registry);

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
        let diags = diagnose_in(Path::new("/work/deep/root"), documents, graph, registry);
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
        let diags = diagnose(documents, graph, registry);

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
                target_col: 9,
            }]),
        );
        documents.insert(
            b.clone(),
            DocumentModel::test("/b.adoc").with_includes(vec![IncludeMacro {
                path: "a.adoc".into(),
                attributes: vec![],
                line: 5,
                target_col: 9,
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

        let diags = diagnose(documents, graph, registry);
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
                target_col: 9,
            }]),
        );

        let graph = IncludeGraph::new();
        let diags = diagnose(documents, graph, registry);
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
                    id: None,
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
                target_col: 9,
            }]),
        );

        let graph = IncludeGraph::new();
        let diags = diagnose(documents, graph, registry);

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
        let diags = diagnose(documents, graph, registry);
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
        let diags = diagnose(documents, graph, registry);
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
                    id: None,
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

        // Asciidoctor knows an ID only within the document it converts: `b.adoc`
        // is part of `a.adoc`, so the xref resolves. Without the include it
        // would be `INFO: possible invalid reference` (see the next test).
        let mut graph = IncludeGraph::new();
        graph.add_edge(
            Path::new("/a.adoc"),
            crate::types::IncludeEdge {
                target: PathBuf::from("/b.adoc"),
                line: 7,
                attributes: vec![],
            },
        );
        let diags = diagnose(documents, graph, registry);
        let flat = flatten(&diags);

        assert!(flat.is_empty(), "Expected no diagnostics, got: {:?}", flat);
    }

    /// Asciidoctor on `b.adoc` alone: `INFO: possible invalid reference:
    /// valid-anchor`. An ID of a document nobody includes is no ID of `b.adoc`.
    #[test]
    fn test_xref_to_id_of_unrelated_top_level_document_is_broken() {
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel::test("/a.adoc").with_anchors(vec![Anchor {
                id: "valid-anchor".into(),
                reftext: None,
                line: 5,
            }]),
        );
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

        let diags = diagnose(documents, IncludeGraph::new(), registry);

        assert!(matches!(
            flatten(&diags)[..],
            [Diagnostic {
                kind: DiagnosticKind::BrokenXref { .. },
                ..
            }]
        ));
    }
    // --- Anchor scope per top-level document (fixture `anchor_scope`) ---

    fn fixture_workspace(name: &str) -> (PathBuf, crate::workspace::Workspace) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(name)
            .canonicalize()
            .unwrap();
        let mut ws = crate::workspace::Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();
        (root, ws)
    }

    fn anchor_scope_workspace() -> (PathBuf, crate::workspace::Workspace) {
        fixture_workspace("anchor_scope")
    }

    fn broken_xrefs<'a>(
        ws: &'a crate::workspace::Workspace,
        file: &Path,
    ) -> Vec<(&'a str, Vec<String>)> {
        ws.diagnostics_for(file)
            .iter()
            .filter_map(|d| match &d.kind {
                DiagnosticKind::BrokenXref { target, documents } => Some((
                    target.as_str(),
                    documents
                        .iter()
                        .map(|doc| doc.file_name().unwrap().to_string_lossy().into_owned())
                        .collect(),
                )),
                _ => None,
            })
            .collect()
    }

    /// Asciidoctor (`web.adoc`): `INFO: possible invalid reference:
    /// setup-only`, nothing for `book.adoc`, where `setup.adoc` defines it.
    /// The shared chapter is checked per top-level document; the message
    /// names the one that lacks the target.
    #[test]
    fn test_broken_xref_in_shared_file_names_documents_where_target_is_missing() {
        let (root, ws) = anchor_scope_workspace();

        let broken = broken_xrefs(&ws, &root.join("chapters/reference.adoc"));

        assert_eq!(broken, vec![("setup-only", vec!["web.adoc".to_string()])]);
    }

    /// Asciidoctor (`book.adoc`): no message. `xref:chapters/reference.adoc#setup-only[]`
    /// is internal because `reference.adoc` is part of the document, and
    /// `_details_2` is the generated ID of the second `== Details`.
    #[test]
    fn test_xrefs_resolving_within_the_top_level_document_are_not_broken() {
        let (root, ws) = anchor_scope_workspace();

        assert_eq!(broken_xrefs(&ws, &root.join("book.adoc")), vec![]);
    }

    /// Asciidoctor (`solo/main.adoc`, `solo2/main.adoc`): no message for
    /// `<<_second>>` in either; each document has its own `_second`. This
    /// pins only that a document's own ID is no broken xref; that another
    /// document's ID is invisible is pinned by
    /// `test_xref_to_id_of_unrelated_top_level_document_is_broken`.
    #[test]
    fn test_bare_xref_to_the_documents_own_id_is_not_broken() {
        let (root, ws) = anchor_scope_workspace();

        assert_eq!(broken_xrefs(&ws, &root.join("solo/main.adoc")), vec![]);
        assert_eq!(broken_xrefs(&ws, &root.join("solo2/main.adoc")), vec![]);
    }

    /// Asciidoctor (`foo.adoc`): `INFO: possible invalid reference: _foo_3`
    /// only; the second `== Foo` is `_foo_2`.
    #[test]
    fn test_generated_id_suffix_resolves_and_the_next_does_not() {
        let (root, ws) = anchor_scope_workspace();

        assert_eq!(
            broken_xrefs(&ws, &root.join("foo.adoc")),
            vec![("_foo_3", vec![])]
        );
    }

    /// Asciidoctor does not check `file#id` into a document that is not part
    /// of the converted one (link, not reference); the LSP does, against the
    /// catalog of that file read as a document of its own, because the link
    /// would be dead in the HTML.
    #[test]
    fn test_file_xref_into_foreign_document_is_checked_against_that_file() {
        let (root, ws) = anchor_scope_workspace();

        assert_eq!(
            broken_xrefs(&ws, &root.join("solo/foreign.adoc")),
            vec![("../chapters/setup.adoc#nope", vec![])]
        );
    }

    /// Asciidoctor (`partial/book.adoc`): `xref:b.adoc#a-id[]` is internal
    /// (`b.adoc` is included in full) and fine; `xref:a.adoc#b-id[]` renders
    /// the link `a.html#b-id` because `a.adoc` is included with `lines=` and
    /// so is no part of the document. Asciidoctor does not check a link; the
    /// LSP checks it against the catalog of `a.adoc` alone, where `b-id` is
    /// missing.
    #[test]
    fn test_file_xref_into_partially_included_file_is_checked_against_that_file() {
        let (root, ws) = anchor_scope_workspace();

        assert_eq!(
            broken_xrefs(&ws, &root.join("partial/book.adoc")),
            vec![("a.adoc#b-id", vec![])]
        );
    }

    /// Asciidoctor (`anchor_selection/book.adoc`): no message, the xref
    /// renders as `href="#b-id"`. `tags=**;!t` reads all but a named tag, which
    /// is no partial include, so `a.adoc` is part of the document and `b-id`
    /// is found in `b.adoc`.
    #[test]
    fn test_file_xref_into_file_included_without_a_named_tag_is_internal() {
        let (root, ws) = fixture_workspace("anchor_selection");

        assert_eq!(broken_xrefs(&ws, &root.join("book.adoc")), vec![]);
    }

    /// Asciidoctor (`anchor_region_xref/web3.adoc`, `reference.adoc[tag=top]`):
    /// no message, `<<setup-only>>` lies outside the tag. `book.adoc` reads
    /// the whole file and finds `setup-only`. The LSP knows no tag regions:
    /// `web3.adoc` reads only a part of the file, so it is no judge of what
    /// is missing there (prefer no diagnostic over a false one).
    #[test]
    fn test_broken_xref_outside_tag_not_reported_lsp_knows_no_tag_regions() {
        let (root, ws) = fixture_workspace("anchor_region_xref");

        let broken = broken_xrefs(&ws, &root.join("chapters/reference.adoc"));

        assert!(
            broken.iter().all(|(target, _)| *target != "setup-only"),
            "{broken:?}"
        );
    }

    /// Asciidoctor (`book.adoc`): `INFO: possible invalid reference: nowhere`;
    /// `web3.adoc` reads only the tag `top`, so it is not named. The xref
    /// inside `tag=inner` that a document reading `tag=inner` would report is
    /// a false negative, accepted.
    #[test]
    fn test_broken_xref_names_only_whole_readers() {
        let (root, ws) = fixture_workspace("anchor_region_xref");

        assert_eq!(
            broken_xrefs(&ws, &root.join("chapters/reference.adoc")),
            vec![("nowhere", vec!["book.adoc".to_string()])]
        );
    }

    /// Asciidoctor (`anchor_region_nested/web.adoc`): `INFO: possible invalid
    /// reference: nowhere`, because `snippet.adoc` lies inside `tag=x` of the
    /// wrapper. The LSP cannot tell, so a file read only below a region
    /// include gets no diagnostic (false negative, accepted).
    #[test]
    fn test_broken_xref_file_read_only_below_region_include_not_reported() {
        let (root, ws) = fixture_workspace("anchor_region_nested");

        assert_eq!(broken_xrefs(&ws, &root.join("snippet.adoc")), vec![]);
    }

    /// Asciidoctor (`anchor_single_master/master.adoc`): `INFO: possible
    /// invalid reference: nope`. With a single reading document the message
    /// names none.
    #[test]
    fn test_broken_xref_in_file_with_one_master_names_no_document() {
        let (root, ws) = fixture_workspace("anchor_single_master");

        assert_eq!(
            broken_xrefs(&ws, &root.join("part.adoc")),
            vec![("nope", vec![])]
        );
    }

    /// Asciidoctor (`a.adoc`, `b.adoc`, both including `s.adoc`): `INFO:
    /// possible invalid reference: nope` in each build. A target missing in
    /// every document that reads the file is still reported with all of
    /// them named (decision: a broken xref is reported per top-level
    /// document).
    #[test]
    fn test_broken_xref_missing_in_every_reading_document_names_all_of_them() {
        let (root, ws) = fixture_workspace("shared_missing");

        assert_eq!(
            broken_xrefs(&ws, &root.join("s.adoc")),
            vec![("nope", vec!["a.adoc".to_string(), "b.adoc".to_string()])]
        );
    }

    /// `s.adoc` is read by `a.adoc` (`edition` is `x`) and `b.adoc` (`y`).
    /// `x/doc.adoc` defines `t`, `y/doc.adoc` does not, so the file part of
    /// the xref is resolved in each document's own scope (as go-to-definition
    /// does) and only `b.adoc` lacks the target. Asciidoctor does not check
    /// such a link; the LSP does, as for any file outside the document.
    #[test]
    fn test_broken_xref_resolves_the_file_part_in_each_reading_documents_scope() {
        let (root, ws) = fixture_workspace("attr_xref");

        assert_eq!(
            broken_xrefs(&ws, &root.join("s.adoc")),
            vec![("{edition}/doc.adoc#t", vec!["b.adoc".to_string()])]
        );
    }

    /// The `DuplicateAnchor`s of `file` as (line, file name of the first
    /// definition, its line, names of the documents).
    fn duplicates(
        ws: &crate::workspace::Workspace,
        file: &Path,
    ) -> Vec<(usize, String, usize, Vec<String>)> {
        let name = |path: &Path| path.file_name().unwrap().to_string_lossy().into_owned();
        ws.diagnostics_for(file)
            .iter()
            .filter_map(|d| match &d.kind {
                DiagnosticKind::DuplicateAnchor {
                    first_file,
                    first_line,
                    documents,
                    ..
                } => Some((
                    d.line,
                    name(first_file),
                    *first_line,
                    documents.iter().map(|doc| name(doc)).collect(),
                )),
                _ => None,
            })
            .collect()
    }

    /// Asciidoctor (`book.adoc`): `WARNING: chapters/reference.adoc: line 2:
    /// id assigned to section already in use: shared`, nothing for `web.adoc`,
    /// which includes `reference.adoc` alone. The warning stands at the second
    /// definition in reading order and names the document that reads both.
    #[test]
    fn test_duplicate_id_across_included_files_is_reported_at_the_second_in_reading_order() {
        let (root, ws) = anchor_scope_workspace();

        assert_eq!(
            duplicates(&ws, &root.join("chapters/reference.adoc")),
            vec![(
                1,
                "setup.adoc".to_string(),
                1,
                vec!["book.adoc".to_string()]
            )]
        );
        assert_eq!(duplicates(&ws, &root.join("chapters/setup.adoc")), vec![]);
    }

    /// Asciidoctor (`standalone.adoc`, `web.adoc`): no warning. Documents that
    /// do not include each other may reuse an ID.
    #[test]
    fn test_same_id_in_unrelated_top_level_documents_is_no_duplicate() {
        let (root, ws) = anchor_scope_workspace();

        assert_eq!(duplicates(&ws, &root.join("standalone.adoc")), vec![]);
    }

    /// Asciidoctor (`pair/x.adoc`, `pair/y.adoc`: one warning each at
    /// `two.adoc`; `pair/z.adoc` reads `two.adoc` first: its warning is at
    /// `one.adoc`). Documents that agree on the place share one diagnostic.
    #[test]
    fn test_duplicate_id_names_every_document_that_reads_both_in_the_same_order() {
        let (root, ws) = anchor_scope_workspace();

        assert_eq!(
            duplicates(&ws, &root.join("pair/two.adoc")),
            vec![(
                1,
                "one.adoc".to_string(),
                1,
                vec!["x.adoc".to_string(), "y.adoc".to_string()]
            )]
        );
        assert_eq!(
            duplicates(&ws, &root.join("pair/one.adoc")),
            vec![(1, "two.adoc".to_string(), 1, vec!["z.adoc".to_string()])]
        );
    }

    /// Asciidoctor (`foo.adoc`): the second `== Foo` is `_foo_2`, no warning.
    #[test]
    fn test_repeated_section_title_is_no_duplicate() {
        let (root, ws) = anchor_scope_workspace();

        assert_eq!(duplicates(&ws, &root.join("foo.adoc")), vec![]);
    }

    /// Asciidoctor (`thrice.adoc`: `part.adoc` with `[[dup]]` included three
    /// times): two warnings, both at `part.adoc`'s line 1 within the one
    /// document. The diagnostic names that document once.
    #[test]
    fn test_duplicate_read_repeatedly_names_the_document_once() {
        let (root, ws) = fixture_workspace("anchor_reread");

        let found = duplicates(&ws, &root.join("part.adoc"));
        let rereads: Vec<_> = found.iter().filter(|d| d.1 == "part.adoc").collect();
        assert_eq!(
            rereads,
            vec![&(
                1,
                "part.adoc".to_string(),
                1,
                vec!["thrice.adoc".to_string()]
            )]
        );
    }

    /// Asciidoctor (`twice_after.adoc`: `first.adoc` defines `dup`, then
    /// `part.adoc` is included twice): two warnings in the one document, so
    /// the one document is named once.
    #[test]
    fn test_duplicate_pair_met_twice_after_a_first_definition_names_the_document_once() {
        let (root, ws) = fixture_workspace("anchor_reread");

        let found = duplicates(&ws, &root.join("part.adoc"));
        let after_first: Vec<_> = found.iter().filter(|d| d.1 == "first.adoc").collect();
        assert_eq!(
            after_first,
            vec![&(
                1,
                "first.adoc".to_string(),
                1,
                vec!["twice_after.adoc".to_string()]
            )]
        );
    }

    /// Asciidoctor (`implicit_then_explicit.adoc`): `== Foo` takes `_foo`,
    /// the later `[[_foo]]` warns at its line.
    #[test]
    fn test_explicit_id_after_generated_one_is_reported_at_the_explicit_id() {
        let (root, ws) = fixture_workspace("anchor_catalog");

        assert_eq!(
            duplicates(&ws, &root.join("implicit_then_explicit.adoc")),
            vec![(
                5,
                "implicit_then_explicit.adoc".to_string(),
                3,
                vec!["implicit_then_explicit.adoc".to_string()]
            )]
        );
    }

    /// Asciidoctor (`explicit_then_implicit.adoc`): `[[_foo]]` first, the
    /// generated ID of `== Foo` becomes `_foo_2`; no warning.
    #[test]
    fn test_generated_id_after_explicit_one_is_no_duplicate() {
        let (root, ws) = fixture_workspace("anchor_catalog");

        assert_eq!(
            duplicates(&ws, &root.join("explicit_then_implicit.adoc")),
            vec![]
        );
    }

    /// `a.adoc` and `b.adoc` include each other (no top-level document), `a`
    /// includes `c.adoc`. Go-to-definition reads `c.adoc` as its own document,
    /// so diagnostics check its xref there only and name no document, as
    /// Asciidoctor on `c.adoc` alone reports `INFO: possible invalid
    /// reference: only-in-a`. (On `a.adoc` Asciidoctor recurses to the
    /// include depth limit; the cycle itself is diagnosed separately.)
    #[test]
    fn test_xref_in_file_included_only_from_a_cycle_is_checked_in_its_own_document() {
        let (root, ws) = fixture_workspace("anchor_cycle");

        assert_eq!(
            broken_xrefs(&ws, &root.join("c.adoc")),
            vec![("only-in-a", vec![])]
        );
    }

    // --- Undefined attributes ---

    /// The undefined-attribute diagnostics as (file name, line, name), sorted.
    fn undefined(diags: &HashMap<PathBuf, Vec<Diagnostic>>) -> Vec<(String, usize, String)> {
        let mut found: Vec<_> = flatten(diags)
            .into_iter()
            .filter_map(|d| match &d.kind {
                DiagnosticKind::UndefinedAttribute { name, .. } => Some((
                    d.file.file_name().unwrap().to_string_lossy().into_owned(),
                    d.line,
                    name.clone(),
                )),
                _ => None,
            })
            .collect();
        found.sort();
        found
    }

    fn undefined_in_workspace(ws: &Workspace) -> Vec<(String, usize, String)> {
        undefined(&ws.diagnostics)
    }

    fn reference(name: &str, line: usize, start_col: usize) -> AttributeRef {
        AttributeRef {
            name: name.into(),
            line,
            start_col,
            end_col: start_col + name.len() + 2,
        }
    }

    fn entry(name: &str, value: &str, line: usize) -> AttributeDef {
        AttributeDef {
            name: name.into(),
            value: value.into(),
            line,
        }
    }

    /// The diagnostics of one top-level document `/a.adoc`.
    fn diagnose_one(doc: DocumentModel) -> HashMap<PathBuf, Vec<Diagnostic>> {
        let mut documents = HashMap::new();
        documents.insert(PathBuf::from("/a.adoc"), doc);
        diagnose(documents, IncludeGraph::new(), make_registry())
    }

    fn a_doc() -> DocumentModel {
        DocumentModel::test("/a.adoc")
    }

    fn lines_and_names(found: Vec<(String, usize, String)>) -> Vec<(usize, String)> {
        found
            .into_iter()
            .map(|(_, line, name)| (line, name))
            .collect()
    }

    #[test]
    fn undefined_attribute_is_warning_with_span() {
        let diags = diagnose_one(a_doc().with_substituted_refs(vec![reference("prodcut", 3, 6)]));
        let flat = flatten(&diags);

        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].line, 3);
        assert_eq!(
            flat[0].span,
            Some(crate::types::Span {
                start_col: 6,
                end_line: 3,
                end_col: 15,
            })
        );
        assert_eq!(
            flat[0].kind,
            DiagnosticKind::UndefinedAttribute {
                name: "prodcut".into(),
                documents: vec![],
            }
        );
    }

    /// Asciidoctor (`g-before-def`): a reference before the entry warns.
    #[test]
    fn defined_before_is_quiet_defined_after_is_reported() {
        let doc = a_doc()
            .with_attributes(vec![entry("product", "Foo", 2), entry("late", "x", 5)])
            .with_substituted_refs(vec![
                reference("product", 3, 0),
                reference("late", 3, 10),
                reference("late", 6, 0),
            ]);

        assert_eq!(
            lines_and_names(undefined(&diagnose_one(doc))),
            vec![(3, "late".to_string())]
        );
    }

    #[test]
    fn unset_entry_makes_reference_undefined() {
        let doc = a_doc()
            .with_attributes(vec![entry("product", "Foo", 2), entry("product!", "", 4)])
            .with_substituted_refs(vec![reference("product", 3, 0), reference("product", 5, 0)]);

        assert_eq!(
            lines_and_names(undefined(&diagnose_one(doc))),
            vec![(5, "product".to_string())]
        );
    }

    /// Asciidoctor: names are lower-cased on both sides.
    #[test]
    fn names_compare_case_insensitively() {
        let doc = a_doc()
            .with_attributes(vec![entry("MixedCase", "one", 1)])
            .with_substituted_refs(vec![
                reference("mixedcase", 2, 0),
                reference("MIXEDCASE", 3, 0),
            ]);

        assert_eq!(undefined(&diagnose_one(doc)), vec![]);
    }

    #[test]
    fn predefined_attributes_are_quiet() {
        let names = [
            "nbsp",
            "docname",
            "toc-title",
            "backend-pdf",
            "env-github",
            "docdir",
            "safe-mode-name",
            "doctitle",
            "empty",
        ];
        let refs = names
            .iter()
            .enumerate()
            .map(|(index, name)| reference(name, index + 1, 0))
            .collect();

        assert_eq!(
            undefined(&diagnose_one(a_doc().with_substituted_refs(refs))),
            vec![]
        );
    }

    /// Asciidoctor warns for `{author}` without a header line, but the LSP
    /// models neither the author nor the revision line: no diagnostic is
    /// better than a false one.
    #[test]
    fn author_attributes_not_reported_lsp_has_no_header_model() {
        let names = [
            "author",
            "email",
            "firstname",
            "lastname",
            "authorinitials",
            "authors",
            "author_2",
            "email_3",
            "revnumber",
            "revdate",
            "revremark",
        ];
        let refs = names
            .iter()
            .enumerate()
            .map(|(index, name)| reference(name, index + 1, 0))
            .collect();

        assert_eq!(
            undefined(&diagnose_one(a_doc().with_substituted_refs(refs))),
            vec![]
        );
    }

    /// Asciidoctor: only `skip` and `drop` silence the warning; `drop-line`
    /// logs an INFO of its own and the LSP warns as well.
    #[rstest::rstest]
    #[case::missing(None, 1)]
    #[case::warn(Some("warn"), 1)]
    #[case::drop_line(Some("drop-line"), 1)]
    #[case::skip(Some("skip"), 0)]
    #[case::drop(Some("drop"), 0)]
    #[case::unknown(Some("bogus"), 1)]
    fn attribute_missing_modes(#[case] mode: Option<&str>, #[case] expected: usize) {
        let mut doc = a_doc().with_substituted_refs(vec![reference("x", 3, 0)]);
        if let Some(mode) = mode {
            doc = doc.with_attributes(vec![entry("attribute-missing", mode, 1)]);
        }

        assert_eq!(undefined(&diagnose_one(doc)).len(), expected);
    }

    #[test]
    fn attribute_missing_applies_from_its_position() {
        let doc = a_doc()
            .with_attributes(vec![entry("attribute-missing", "skip", 2)])
            .with_substituted_refs(vec![reference("x", 1, 0), reference("x", 3, 0)]);

        assert_eq!(
            lines_and_names(undefined(&diagnose_one(doc))),
            vec![(1, "x".to_string())]
        );
    }

    #[test]
    fn unsetting_attribute_missing_warns_again() {
        let doc = a_doc()
            .with_attributes(vec![
                entry("attribute-missing", "skip", 1),
                entry("attribute-missing!", "", 3),
            ])
            .with_substituted_refs(vec![reference("x", 2, 0), reference("x", 4, 0)]);

        assert_eq!(
            lines_and_names(undefined(&diagnose_one(doc))),
            vec![(4, "x".to_string())]
        );
    }

    #[test]
    fn attribute_missing_from_config_chain() {
        let (_, ws) = fixture_workspace("undefined_attributes_skip");

        assert_eq!(undefined_in_workspace(&ws), vec![]);
    }

    #[test]
    fn possible_definition_quiets_later_reference_only() {
        let doc = a_doc()
            .with_possible_definitions(vec![crate::types::PossibleDefinition {
                name: "x".into(),
                line: 2,
            }])
            .with_attributes(vec![entry("x!", "", 4)])
            .with_substituted_refs(vec![
                reference("x", 1, 0),
                reference("x", 3, 0),
                reference("x", 5, 0),
            ]);

        assert_eq!(
            lines_and_names(undefined(&diagnose_one(doc))),
            vec![(1, "x".to_string()), (5, "x".to_string())]
        );
    }

    /// Asciidoctor (`:a: {a}` with `a` unset before): the reference on an
    /// entry line means the value before the entry.
    #[test]
    fn reference_on_its_own_entry_line_is_reported() {
        let doc = a_doc()
            .with_attributes(vec![entry("a", "{a}", 1)])
            .with_substituted_refs(vec![reference("a", 1, 4)]);

        assert_eq!(
            lines_and_names(undefined(&diagnose_one(doc))),
            vec![(1, "a".to_string())]
        );
    }

    #[test]
    fn possible_attribute_missing_silences_later_references() {
        let doc = a_doc()
            .with_possible_definitions(vec![crate::types::PossibleDefinition {
                name: "attribute-missing".into(),
                line: 1,
            }])
            .with_substituted_refs(vec![reference("x", 2, 0)]);

        assert_eq!(undefined(&diagnose_one(doc)), vec![]);
    }

    /// An entry on a line a block conditional covers may not run, so an
    /// unset there does not make the attribute undefined.
    #[test]
    fn unset_inside_block_conditional_is_not_reported() {
        let doc = a_doc()
            .with_attributes(vec![entry("product", "Foo", 1), entry("product!", "", 3)])
            .with_possible_definitions(vec![crate::types::PossibleDefinition {
                name: "product".into(),
                line: 3,
            }])
            .with_substituted_refs(vec![reference("product", 5, 0)]);

        assert_eq!(undefined(&diagnose_one(doc)), vec![]);
    }

    #[test]
    fn file_included_twice_reports_once() {
        let mut documents = HashMap::new();
        documents.insert(PathBuf::from("/a.adoc"), a_doc());
        documents.insert(
            PathBuf::from("/b.adoc"),
            DocumentModel::test("/b.adoc").with_substituted_refs(vec![reference("y", 1, 0)]),
        );
        let mut graph = IncludeGraph::new();
        for line in [2, 4] {
            graph.add_edge(
                Path::new("/a.adoc"),
                crate::types::IncludeEdge {
                    target: PathBuf::from("/b.adoc"),
                    line,
                    attributes: vec![],
                },
            );
        }

        let diags = diagnose(documents, graph, make_registry());

        assert_eq!(
            undefined(&diags),
            vec![("b.adoc".to_string(), 1, "y".to_string())]
        );
    }

    /// Asciidoctor on each top-level document: `book.adoc` defines the
    /// edition and is quiet, `web.adoc` warns (once per position, although
    /// it reads the file twice). `excerpt.adoc` reads only a tag of the
    /// file and is no judge: it is not named.
    #[test]
    fn shared_file_is_checked_per_top_level_document() {
        let (root, ws) = fixture_workspace("undefined_attributes_shared");

        let reported: Vec<_> = ws
            .diagnostics_for(&root.join("shared.adoc"))
            .iter()
            .filter_map(|d| match &d.kind {
                DiagnosticKind::UndefinedAttribute { name, documents } => Some((
                    d.line,
                    name.clone(),
                    documents
                        .iter()
                        .map(|doc| doc.file_name().unwrap().to_string_lossy().into_owned())
                        .collect::<Vec<_>>(),
                )),
                _ => None,
            })
            .collect();

        assert_eq!(
            reported.len(),
            2,
            "one diagnostic per position, got {reported:?}"
        );
        for (_, name, documents) in &reported {
            assert_eq!(name, "edition");
            assert_eq!(documents, &vec!["web.adoc".to_string()]);
        }
        assert_eq!(
            reported.iter().map(|(line, ..)| *line).collect::<Vec<_>>(),
            vec![3, 6]
        );
    }

    /// Asciidoctor (`index.adoc` with `-a attribute-missing=warn`): the same
    /// five warnings; everything else in the fixture stays quiet. The
    /// chapter sees `imagesdir` from the master.
    #[test]
    fn test_undefined_attributes_fixture() {
        let (_, ws) = fixture_workspace("undefined_attributes");

        assert_eq!(
            undefined_in_workspace(&ws),
            vec![
                (
                    "chapter.adoc".to_string(),
                    3,
                    "missing-in-chapter".to_string()
                ),
                ("index.adoc".to_string(), 5, "prodcut".to_string()),
                ("index.adoc".to_string(), 9, "product".to_string()),
                ("index.adoc".to_string(), 11, "host".to_string()),
                ("index.adoc".to_string(), 15, "inlisting".to_string()),
            ]
        );
    }

    /// The config defines `product` and `company` (ADR-008); the
    /// Asciidoctor CLI does not read `.asciidoctorconfig.adoc` and warns.
    #[test]
    fn test_adversarial_project_has_no_undefined_attributes() {
        let (_, ws) = fixture_workspace("adversarial_project");

        assert_eq!(undefined_in_workspace(&ws), vec![]);
    }
}
