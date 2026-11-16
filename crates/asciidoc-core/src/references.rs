use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::attributes::AttributeRegistry;
use crate::graph::IncludeGraph;
use crate::paths::resolve_doc_target;
use crate::types::{
    AnchorKind, AnchorLocation, DiagnosticKind, DocumentModel, ResolvedXref, Section, Xref,
};

pub struct ReferenceIndex {
    pub(crate) anchors: HashMap<String, AnchorLocation>,
    pub(crate) duplicates: Vec<(String, AnchorLocation)>,
    pub(crate) by_file: HashMap<PathBuf, Vec<String>>,
}

impl Default for ReferenceIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl ReferenceIndex {
    /// All anchors keyed by ID.
    #[must_use]
    pub fn anchors(&self) -> &HashMap<String, AnchorLocation> {
        &self.anchors
    }

    /// Duplicate anchors (same ID within the same file).
    #[must_use]
    pub fn duplicates(&self) -> &[(String, AnchorLocation)] {
        &self.duplicates
    }

    #[must_use]
    pub fn new() -> Self {
        Self {
            anchors: HashMap::new(),
            duplicates: Vec::new(),
            by_file: HashMap::new(),
        }
    }

    pub fn add_anchor(&mut self, id: &str, location: AnchorLocation) {
        self.by_file
            .entry(location.file.clone())
            .or_default()
            .push(id.to_string());

        if let Some(existing) = self.anchors.get(id) {
            if existing.file == location.file {
                // Duplicate within same file — real duplicate
                self.duplicates.push((id.to_string(), location));
            }
            // Different files — not a duplicate (section IDs are per-document in AsciiDoc)
        } else {
            self.anchors.insert(id.to_string(), location);
        }
    }

    #[allow(dead_code)] // Public API for LSP consumers
    #[must_use]
    pub fn lookup(&self, id: &str) -> Option<&AnchorLocation> {
        self.anchors.get(id)
    }

    /// Add a single file's anchors and implicit section IDs to the index,
    /// treating the file as a top-level document (no ID for its header
    /// title). `Workspace` knows the include graph and adds the header title
    /// ID of included files.
    ///
    /// Call `remove_file()` first if the file was already indexed.
    pub fn add_file(&mut self, path: &Path, doc: &DocumentModel, registry: &AttributeRegistry) {
        // Explicit anchors
        for anchor in &doc.anchors {
            self.add_anchor(
                &anchor.id,
                AnchorLocation {
                    file: path.to_path_buf(),
                    line: anchor.line,
                    kind: AnchorKind::Explicit,
                },
            );
        }

        // Implicit section IDs
        let prefix = registry.resolve(path, "idprefix").unwrap_or("_");
        let separator = registry.resolve(path, "idseparator").unwrap_or("_");
        // The document header title gets no ID; see `add_header_title_id`.
        for section in doc
            .sections
            .iter()
            .filter(|s| !s.explicit_id && !s.is_header)
        {
            self.add_section_id(path, section, prefix, separator);
        }
    }

    /// Add the implicit ID of the file's header title (`= Title`).
    ///
    /// Asciidoctor generates none for the document header, so `add_file`
    /// skips it. In a file another document includes the title is a section
    /// and keeps its ID: the workspace, which knows the include graph, calls
    /// this for such files.
    pub fn add_header_title_id(
        &mut self,
        path: &Path,
        doc: &DocumentModel,
        registry: &AttributeRegistry,
    ) {
        let Some(section) = doc.sections.first().filter(|s| s.is_header) else {
            return;
        };
        if section.explicit_id {
            return;
        }
        let prefix = registry.resolve(path, "idprefix").unwrap_or("_");
        let separator = registry.resolve(path, "idseparator").unwrap_or("_");
        self.add_section_id(path, section, prefix, separator);
    }

    fn add_section_id(&mut self, path: &Path, section: &Section, prefix: &str, separator: &str) {
        let id = generate_section_id(&section.title, prefix, separator);
        self.add_anchor(
            &id,
            AnchorLocation {
                file: path.to_path_buf(),
                line: section.line,
                kind: AnchorKind::ImplicitSection,
            },
        );
    }

    /// Remove all anchors belonging to a file from the index.
    ///
    /// Handles duplicates: if the removed file owned a primary anchor entry
    /// and a duplicate exists from another file, the duplicate is promoted.
    pub fn remove_file(&mut self, path: &Path) {
        let Some(ids) = self.by_file.remove(path) else {
            return;
        };

        for id in &ids {
            // Check if this file owns the primary entry
            if let Some(loc) = self.anchors.get(id) {
                if loc.file == path {
                    self.anchors.remove(id);

                    // Promote a duplicate if one exists
                    if let Some(pos) = self.duplicates.iter().position(|(did, _)| did == id) {
                        let (dup_id, dup_loc) = self.duplicates.remove(pos);
                        self.anchors.insert(dup_id, dup_loc);
                    }
                }
            }

            // Remove any remaining duplicates from this file
            self.duplicates
                .retain(|(did, dloc)| !(did == id && dloc.file == path));
        }
    }

    /// Build the reference index from all parsed documents.
    /// Collects explicit anchors and generates implicit section IDs.
    #[must_use]
    pub fn build(
        documents: &HashMap<PathBuf, DocumentModel>,
        registry: &AttributeRegistry,
    ) -> Self {
        let mut index = Self::new();

        for (path, doc) in documents {
            index.add_file(path, doc, registry);
        }

        index
    }

    /// Resolve an xref target to a file and line.
    /// Supports: `file.adoc#anchor`, `#anchor`, `anchor` (workspace-wide lookup).
    ///
    /// # Errors
    /// Returns a `DiagnosticKind::BrokenXref` if the target file or anchor cannot be found.
    pub fn resolve_xref(
        &self,
        xref: &Xref,
        source_file: &Path,
        _graph: &IncludeGraph,
        documents: &HashMap<PathBuf, DocumentModel>,
        registry: &AttributeRegistry,
    ) -> Result<ResolvedXref, DiagnosticKind> {
        self.resolve_xref_with(xref, documents, |file| {
            resolve_doc_target(source_file, file, registry)
        })
    }

    /// [`Self::resolve_xref`] with the file part of the target resolved by
    /// `resolve_file` (attribute substitution and joining onto the
    /// document directory), so the caller picks the attribute scope.
    ///
    /// # Errors
    /// Returns a `DiagnosticKind::BrokenXref` if the target file or anchor cannot be found.
    pub fn resolve_xref_with(
        &self,
        xref: &Xref,
        documents: &HashMap<PathBuf, DocumentModel>,
        resolve_file: impl FnOnce(&str) -> Option<PathBuf>,
    ) -> Result<ResolvedXref, DiagnosticKind> {
        let (file_part, anchor_part) = parse_xref_target(&xref.target);

        if let Some(file_ref) = file_part {
            // xref:file.adoc#anchor[] or xref:file.adoc[]
            //
            // Resolution mirrors Asciidoctor:
            // 1. Substitute attributes + resolve relative to the source file's
            //    directory (`paths::resolve_doc_target`). This is the primary
            //    path — it handles `../sibling/doc.adoc`, `sub/doc.adoc`, and
            //    `{docdir}/foo.adoc`.
            // 2. Fall back to a workspace-wide lookup by trailing components
            //    or bare filename, so `xref:doc.adoc[]` keeps working from any
            //    depth.
            let resolved_path = resolve_file(file_ref);
            let target_file = resolved_path
                .as_ref()
                .and_then(|p| documents.keys().find(|key| *key == p))
                .or_else(|| {
                    documents.keys().find(|p| {
                        p.ends_with(file_ref)
                            || p.file_name()
                                .is_some_and(|f| f.to_string_lossy() == file_ref)
                    })
                });

            let path = target_file.ok_or_else(|| DiagnosticKind::BrokenXref {
                target: xref.target.clone(),
            })?;

            if let Some(anchor_id) = anchor_part {
                // Look up anchor in that specific file
                self.anchors
                    .get(anchor_id)
                    .filter(|loc| loc.file == *path)
                    .map(|loc| ResolvedXref {
                        file: path.clone(),
                        line: loc.line,
                    })
                    .ok_or_else(|| DiagnosticKind::BrokenXref {
                        target: xref.target.clone(),
                    })
            } else {
                // File reference without anchor → line 1
                Ok(ResolvedXref {
                    file: path.clone(),
                    line: 1,
                })
            }
        } else if let Some(anchor_id) = anchor_part {
            // Anchor-only: workspace-wide lookup
            match self.anchors.get(anchor_id) {
                Some(loc) => Ok(ResolvedXref {
                    file: loc.file.clone(),
                    line: loc.line,
                }),
                None => Err(DiagnosticKind::BrokenXref {
                    target: xref.target.clone(),
                }),
            }
        } else {
            Err(DiagnosticKind::BrokenXref {
                target: xref.target.clone(),
            })
        }
    }
}

/// Parse an xref target into optional file part and optional anchor part.
/// Examples:
///   "file.adoc#anchor" → (Some("file.adoc"), Some("anchor"))
///   "#anchor"          → (None, Some("anchor"))
///   "file.adoc"        → (Some("file.adoc"), None)
///   "anchor"           → (None, Some("anchor"))
fn parse_xref_target(target: &str) -> (Option<&str>, Option<&str>) {
    if let Some((file, anchor)) = target.split_once('#') {
        let file_part = if file.is_empty() { None } else { Some(file) };
        let anchor_part = if anchor.is_empty() {
            None
        } else {
            Some(anchor)
        };
        (file_part, anchor_part)
    } else if target.contains('.') {
        // Looks like a filename (has extension)
        (Some(target), None)
    } else {
        // Plain anchor ID
        (None, Some(target))
    }
}

/// Generate an implicit section ID from a title.
/// Rules: lowercase, non-alnum → separator, prepend prefix.
#[must_use]
pub fn generate_section_id(title: &str, prefix: &str, separator: &str) -> String {
    let mut result = String::with_capacity(prefix.len() + title.len());
    result.push_str(prefix);

    let mut prev_was_sep = !prefix.is_empty(); // avoid double separator after prefix
    for ch in title.chars() {
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                result.push(lower);
            }
            prev_was_sep = false;
        } else if !prev_was_sep {
            result.push_str(separator);
            prev_was_sep = true;
        }
    }

    // Remove trailing separator
    while result.ends_with(separator) && result.len() > prefix.len() {
        result.truncate(result.len() - separator.len());
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Anchor, Section};
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    // --- Section ID generation ---

    #[rstest]
    #[case("Building Block View", "_", "_", "_building_block_view")]
    #[case("What is \"Clean Code\"?", "_", "_", "_what_is_clean_code")]
    #[case("Building Block View", "sec-", "-", "sec-building-block-view")]
    #[case("Building Block View", "", "_", "building_block_view")]
    fn test_generate_section_id(
        #[case] title: &str,
        #[case] prefix: &str,
        #[case] separator: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(generate_section_id(title, prefix, separator), expected);
    }

    // --- Anchor collection ---

    #[test]
    fn test_add_and_lookup_anchor() {
        let mut index = ReferenceIndex::new();
        index.add_anchor(
            "my-anchor",
            AnchorLocation {
                file: PathBuf::from("/a.adoc"),
                line: 10,
                kind: AnchorKind::Explicit,
            },
        );

        let loc = index.lookup("my-anchor").unwrap();
        assert_eq!(loc.file, PathBuf::from("/a.adoc"));
        assert_eq!(loc.line, 10);
    }

    #[test]
    fn test_by_file_index() {
        let mut index = ReferenceIndex::new();
        let file_a = PathBuf::from("/a.adoc");
        let file_b = PathBuf::from("/b.adoc");

        index.add_anchor(
            "a1",
            AnchorLocation {
                file: file_a.clone(),
                line: 1,
                kind: AnchorKind::Explicit,
            },
        );
        index.add_anchor(
            "a2",
            AnchorLocation {
                file: file_a.clone(),
                line: 5,
                kind: AnchorKind::Explicit,
            },
        );
        index.add_anchor(
            "b1",
            AnchorLocation {
                file: file_b.clone(),
                line: 1,
                kind: AnchorKind::Explicit,
            },
        );

        assert_eq!(index.by_file.get(&file_a).unwrap().len(), 2);
        assert_eq!(index.by_file.get(&file_b).unwrap().len(), 1);
    }

    // --- Index build ---

    #[test]
    fn test_build_index_explicit_anchors() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        let mut documents = HashMap::new();

        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel::test("/a.adoc").with_anchors(vec![
                Anchor {
                    id: "a1".into(),
                    reftext: None,
                    line: 1,
                },
                Anchor {
                    id: "a2".into(),
                    reftext: None,
                    line: 5,
                },
            ]),
        );
        documents.insert(
            PathBuf::from("/b.adoc"),
            DocumentModel::test("/b.adoc").with_anchors(vec![
                Anchor {
                    id: "b1".into(),
                    reftext: None,
                    line: 1,
                },
                Anchor {
                    id: "b2".into(),
                    reftext: None,
                    line: 3,
                },
            ]),
        );

        let index = ReferenceIndex::build(&documents, &registry);
        assert_eq!(index.anchors.len(), 4);
    }

    #[test]
    fn test_build_index_implicit_sections() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        let mut documents = HashMap::new();

        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel::test("/a.adoc").with_sections(vec![Section {
                level: 1,
                title: "Building Block View".into(),
                line: 10,
                explicit_id: false,
                is_header: false,
            }]),
        );

        let index = ReferenceIndex::build(&documents, &registry);
        assert!(index.anchors.contains_key("_building_block_view"));
    }

    #[test]
    fn test_same_anchor_in_different_files_is_not_duplicate() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
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
                line: 5,
            }]),
        );

        let index = ReferenceIndex::build(&documents, &registry);
        // Same anchor ID in different files is NOT a duplicate
        assert!(index.duplicates.is_empty());
    }

    #[test]
    fn test_same_anchor_in_same_file_is_duplicate() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
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

        let index = ReferenceIndex::build(&documents, &registry);
        assert_eq!(index.duplicates.len(), 1);
        assert_eq!(index.duplicates[0].0, "dup");
    }

    // --- Xref resolution ---

    #[test]
    fn test_resolve_xref_file_anchor() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/file.adoc"),
            DocumentModel::test("/file.adoc").with_anchors(vec![Anchor {
                id: "anchor".into(),
                reftext: None,
                line: 42,
            }]),
        );

        let index = ReferenceIndex::build(&documents, &registry);
        let graph = IncludeGraph::new();

        let xref = Xref {
            target: "file.adoc#anchor".into(),
            display_text: None,
            line: 1,
            span: None,
            target_col: 0,
        };
        let result = index
            .resolve_xref(&xref, Path::new("/src.adoc"), &graph, &documents, &registry)
            .unwrap();
        assert_eq!(result.file, PathBuf::from("/file.adoc"));
        assert_eq!(result.line, 42);
    }

    #[test]
    fn test_resolve_xref_anchor_only() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/doc.adoc"),
            DocumentModel::test("/doc.adoc").with_sections(vec![Section {
                level: 1,
                title: "My Section".into(),
                line: 5,
                explicit_id: false,
                is_header: false,
            }]),
        );

        let index = ReferenceIndex::build(&documents, &registry);
        let graph = IncludeGraph::new();

        let xref = Xref {
            target: "_my_section".into(),
            display_text: None,
            line: 1,
            span: None,
            target_col: 0,
        };
        let result = index
            .resolve_xref(
                &xref,
                Path::new("/other.adoc"),
                &graph,
                &documents,
                &registry,
            )
            .unwrap();
        assert_eq!(result.file, PathBuf::from("/doc.adoc"));
        assert_eq!(result.line, 5);
    }

    #[test]
    fn test_resolve_xref_broken() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        let index = ReferenceIndex::new();
        let graph = IncludeGraph::new();
        let documents = HashMap::new();

        let xref = Xref {
            target: "nonexistent-id".into(),
            display_text: None,
            line: 1,
            span: None,
            target_col: 0,
        };
        let result =
            index.resolve_xref(&xref, Path::new("/src.adoc"), &graph, &documents, &registry);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            DiagnosticKind::BrokenXref { .. }
        ));
    }

    #[test]
    fn test_resolve_xref_file_no_anchor() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/target.adoc"),
            DocumentModel::test("/target.adoc"),
        );

        let index = ReferenceIndex::build(&documents, &registry);
        let graph = IncludeGraph::new();

        let xref = Xref {
            target: "target.adoc".into(),
            display_text: None,
            line: 1,
            span: None,
            target_col: 0,
        };
        let result = index
            .resolve_xref(&xref, Path::new("/src.adoc"), &graph, &documents, &registry)
            .unwrap();
        assert_eq!(result.file, PathBuf::from("/target.adoc"));
        assert_eq!(result.line, 1);
    }

    #[test]
    fn test_resolve_xref_sibling_directory() {
        // Regression: xref:../sibling/doc.adoc#anchor[] must resolve, not be reported as broken.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        let source = root.join("a/src.adoc");
        let target = root.join("b/doc.adoc");
        std::fs::write(&source, "").unwrap();
        std::fs::write(&target, "").unwrap();

        let registry = AttributeRegistry::new(&root);
        let mut documents = HashMap::new();
        documents.insert(
            target.clone(),
            DocumentModel::test(target.to_str().unwrap()).with_anchors(vec![Anchor {
                id: "my-anchor".into(),
                reftext: None,
                line: 7,
            }]),
        );

        let index = ReferenceIndex::build(&documents, &registry);
        let graph = IncludeGraph::new();

        let xref = Xref {
            target: "../b/doc.adoc#my-anchor".into(),
            display_text: None,
            line: 1,
            span: None,
            target_col: 0,
        };
        let resolved = index
            .resolve_xref(&xref, &source, &graph, &documents, &registry)
            .unwrap();
        assert_eq!(resolved.file, target);
        assert_eq!(resolved.line, 7);
    }

    #[test]
    fn test_resolve_xref_attribute_substituted_target() {
        // xref:{docdir}/sub/doc.adoc#anchor[] must substitute {docdir} and resolve.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let source = root.join("index.adoc");
        let target = root.join("sub/doc.adoc");
        std::fs::write(&source, "").unwrap();
        std::fs::write(&target, "").unwrap();

        let registry = AttributeRegistry::new(&root);
        let mut documents = HashMap::new();
        documents.insert(
            target.clone(),
            DocumentModel::test(target.to_str().unwrap()).with_anchors(vec![Anchor {
                id: "x".into(),
                reftext: None,
                line: 3,
            }]),
        );

        let index = ReferenceIndex::build(&documents, &registry);
        let graph = IncludeGraph::new();

        let xref = Xref {
            target: "{docdir}/sub/doc.adoc#x".into(),
            display_text: None,
            line: 1,
            span: None,
            target_col: 0,
        };
        let resolved = index
            .resolve_xref(&xref, &source, &graph, &documents, &registry)
            .unwrap();
        assert_eq!(resolved.file, target);
        assert_eq!(resolved.line, 3);
    }

    /// The caller decides where the file part of an xref points (one
    /// attribute scope, ADR-4); the anchor lookup stays the same.
    #[test]
    fn test_resolve_xref_with_caller_resolved_file() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/web/doc.adoc"),
            DocumentModel::test("/web/doc.adoc").with_anchors(vec![Anchor {
                id: "x".into(),
                reftext: None,
                line: 5,
            }]),
        );
        let index = ReferenceIndex::build(&documents, &registry);
        let xref = Xref {
            target: "{edition}/doc.adoc#x".into(),
            display_text: None,
            line: 1,
            span: None,
            target_col: 0,
        };

        let resolved = index
            .resolve_xref_with(&xref, &documents, |file| {
                (file == "{edition}/doc.adoc").then(|| PathBuf::from("/web/doc.adoc"))
            })
            .unwrap();

        assert_eq!(resolved.file, PathBuf::from("/web/doc.adoc"));
        assert_eq!(resolved.line, 5);
    }

    // --- Incremental updates (add_file / remove_file) ---

    #[test]
    fn test_reference_index_add_file() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        let mut index = ReferenceIndex::new();

        let file_a = PathBuf::from("/a.adoc");
        let doc_a = DocumentModel::test("/a.adoc").with_anchors(vec![Anchor {
            id: "foo".into(),
            reftext: None,
            line: 1,
        }]);

        index.add_file(&file_a, &doc_a, &registry);

        assert_eq!(index.anchors.len(), 1);
        assert!(index.anchors.contains_key("foo"));
        assert_eq!(index.by_file.get(&file_a).unwrap().len(), 1);
    }

    #[test]
    fn test_reference_index_remove_file() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        let mut index = ReferenceIndex::new();

        let file_a = PathBuf::from("/a.adoc");
        let doc_a = DocumentModel::test("/a.adoc").with_anchors(vec![
            Anchor {
                id: "a1".into(),
                reftext: None,
                line: 1,
            },
            Anchor {
                id: "a2".into(),
                reftext: None,
                line: 5,
            },
        ]);

        index.add_file(&file_a, &doc_a, &registry);
        assert_eq!(index.anchors.len(), 2);

        index.remove_file(&file_a);
        assert!(index.anchors.is_empty());
        assert!(index.by_file.get(&file_a).is_none());
    }

    #[test]
    fn test_reference_index_incremental_update() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        let mut index = ReferenceIndex::new();

        let file_a = PathBuf::from("/a.adoc");
        let file_b = PathBuf::from("/b.adoc");

        // Add file A with anchor "foo"
        let doc_a = DocumentModel::test("/a.adoc").with_anchors(vec![Anchor {
            id: "foo".into(),
            reftext: None,
            line: 1,
        }]);
        index.add_file(&file_a, &doc_a, &registry);

        // Add file B with anchor "bar"
        let doc_b = DocumentModel::test("/b.adoc").with_anchors(vec![Anchor {
            id: "bar".into(),
            reftext: None,
            line: 1,
        }]);
        index.add_file(&file_b, &doc_b, &registry);

        assert_eq!(index.anchors.len(), 2);
        assert!(index.lookup("foo").is_some());
        assert!(index.lookup("bar").is_some());

        // Remove file B — only "foo" remains
        index.remove_file(&file_b);
        assert_eq!(index.anchors.len(), 1);
        assert!(index.lookup("foo").is_some());
        assert!(index.lookup("bar").is_none());
    }

    #[test]
    fn test_reference_index_remove_file_same_id_cross_file() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        let mut index = ReferenceIndex::new();

        let file_a = PathBuf::from("/a.adoc");
        let file_b = PathBuf::from("/b.adoc");

        // Both files define "context" — not a duplicate (cross-file)
        let doc_a = DocumentModel::test("/a.adoc").with_anchors(vec![Anchor {
            id: "context".into(),
            reftext: None,
            line: 1,
        }]);
        let doc_b = DocumentModel::test("/b.adoc").with_anchors(vec![Anchor {
            id: "context".into(),
            reftext: None,
            line: 5,
        }]);

        index.add_file(&file_a, &doc_a, &registry);
        index.add_file(&file_b, &doc_b, &registry);

        // Cross-file: no duplicates, first file wins in anchors lookup
        assert!(index.duplicates.is_empty());
        assert_eq!(index.anchors.get("context").unwrap().file, file_a);

        // Remove file A — anchor entry gone, by_file cleaned
        index.remove_file(&file_a);
        assert!(index.anchors.get("context").is_none());
        assert!(index.by_file.get(&file_a).is_none());
    }

    #[test]
    fn test_reference_index_remove_file_with_same_file_duplicates() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        let mut index = ReferenceIndex::new();

        let file_a = PathBuf::from("/a.adoc");

        // Same file, same anchor twice — real duplicate
        let doc_a = DocumentModel::test("/a.adoc").with_anchors(vec![
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
        ]);

        index.add_file(&file_a, &doc_a, &registry);
        assert_eq!(index.duplicates.len(), 1);

        // Remove file — both primary and duplicate gone
        index.remove_file(&file_a);
        assert!(index.anchors.get("dup").is_none());
        assert!(index.duplicates.is_empty());
    }

    #[test]
    fn test_reference_index_add_file_with_sections() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        let mut index = ReferenceIndex::new();

        let file_a = PathBuf::from("/a.adoc");
        let doc_a = DocumentModel::test("/a.adoc").with_sections(vec![Section {
            level: 1,
            title: "My Section".into(),
            line: 3,
            explicit_id: false,
            is_header: false,
        }]);

        index.add_file(&file_a, &doc_a, &registry);

        // Implicit section ID generated
        assert!(index.anchors.contains_key("_my_section"));
        assert_eq!(index.by_file.get(&file_a).unwrap().len(), 1);
    }

    // --- Committed fixtures: tests/fixtures/{explicit_section_ids,book_parts,leveloffset_*,nested_include,book_part_include} ---
    //
    // Asciidoctor generates no `_title` ID for a section that carries an
    // explicit ID, nor for the document header title. The xref completion
    // reads `reference_index()`, so the index assertions cover it too.

    fn fixture_workspace(name: &str) -> crate::workspace::Workspace {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(name);
        crate::workspace::Workspace::build(&root, &root.join("index.adoc"))
    }

    fn broken_targets(ws: &crate::workspace::Workspace) -> Vec<String> {
        let mut broken: Vec<String> = ws
            .all_diagnostics()
            .into_iter()
            .filter_map(|d| match d.kind {
                crate::types::DiagnosticKind::BrokenXref { target } => Some(target),
                _ => None,
            })
            .collect();
        broken.sort();
        broken
    }

    #[test]
    fn test_fixture_explicit_section_ids_suppress_implicit_ids() {
        let ws = fixture_workspace("explicit_section_ids");

        assert_eq!(
            broken_targets(&ws),
            [
                "_audience",
                "_by_attr",
                "_inline_anchor_tid",
                "_introduction",
                "_product_manual",
                "_stacked",
                "nsp"
            ]
        );
        // Not an anchor in Asciidoctor (escaped, invalid ID start, no space
        // before `[[`): the section keeps its implicit ID.
        for id in ["_escaped_esc", "_bad_anchor_1bad", "_nospace"] {
            assert!(ws.reference_index().lookup(id).is_some(), "{id}");
        }
        // Attribute lists without an ID keep the implicit ID.
        for id in ["_options", "_with_role", "_appx"] {
            assert!(ws.reference_index().lookup(id).is_some(), "{id}");
        }
        assert!(ws.reference_index().lookup("_introduction").is_none());
        assert!(ws.reference_index().lookup("tid").is_some());
        // `[[id,]]` (empty reftext) is no section anchor: the anchor text
        // becomes part of the title.
        assert!(ws.reference_index().lookup("_empty_reftext_etid").is_some());
        assert!(ws.reference_index().lookup("etid").is_none());
    }

    #[test]
    fn test_fixture_included_title_with_block_id_has_no_implicit_id() {
        let ws = fixture_workspace("included_title_block_id");

        assert_eq!(broken_targets(&ws), ["_ch_one", "_ch_three", "_ch_two"]);
        // `[#id]` / `[id=]` are not indexed as anchors yet (separate backlog
        // item), so only `[[cid1]]` is asserted as an anchor.
        for id in ["cid1", "_ch_four_with_role"] {
            assert!(ws.reference_index().lookup(id).is_some(), "{id}");
        }
        assert!(ws.reference_index().lookup("_ch_one").is_none());
    }

    #[test]
    fn test_fixture_headerless_book_part_keeps_implicit_id() {
        let ws = fixture_workspace("headerless_book");

        assert!(broken_targets(&ws).is_empty());
        assert!(ws.reference_index().lookup("_part_one").is_some());
    }

    #[test]
    fn test_fixture_book_part_keeps_implicit_id_header_title_does_not() {
        let ws = fixture_workspace("book_parts");

        assert_eq!(broken_targets(&ws), ["_my_book"]);
        assert!(ws.reference_index().lookup("_part_one").is_some());
    }

    #[test]
    fn test_fixture_leveloffset_argument_include_keeps_title_id() {
        let ws = fixture_workspace("leveloffset_chapter");

        assert_eq!(broken_targets(&ws), ["_main_doc"]);
        assert!(ws.reference_index().lookup("_chapter_foo").is_some());
    }

    #[test]
    fn test_fixture_leveloffset_attribute_include_keeps_title_id() {
        let ws = fixture_workspace("leveloffset_attribute");

        assert!(broken_targets(&ws).is_empty());
        assert!(ws.reference_index().lookup("_chapter_foo").is_some());
    }

    #[test]
    fn test_fixture_nested_include_keeps_title_id() {
        let ws = fixture_workspace("nested_include");

        assert!(broken_targets(&ws).is_empty());
        assert!(ws.reference_index().lookup("_inner_title").is_some());
    }

    #[test]
    fn test_fixture_book_part_include_keeps_title_id() {
        let ws = fixture_workspace("book_part_include");

        assert!(broken_targets(&ws).is_empty());
        assert!(ws.reference_index().lookup("_chapter_foo").is_some());
    }
}
