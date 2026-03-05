use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::attributes::AttributeRegistry;
use crate::graph::IncludeGraph;
use crate::types::{AnchorKind, AnchorLocation, DiagnosticKind, DocumentModel, ResolvedXref, Xref};

pub struct ReferenceIndex {
    pub anchors: HashMap<String, AnchorLocation>,
    pub duplicates: Vec<(String, AnchorLocation)>,
    pub by_file: HashMap<PathBuf, Vec<String>>,
}

impl Default for ReferenceIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl ReferenceIndex {
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

        if self.anchors.contains_key(id) {
            self.duplicates.push((id.to_string(), location));
        } else {
            self.anchors.insert(id.to_string(), location);
        }
    }

    #[allow(dead_code)] // Public API for LSP consumers
    #[must_use]
    pub fn lookup(&self, id: &str) -> Option<&AnchorLocation> {
        self.anchors.get(id)
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
            // Explicit anchors
            for anchor in &doc.anchors {
                index.add_anchor(
                    &anchor.id,
                    AnchorLocation {
                        file: path.clone(),
                        line: anchor.line,
                        kind: AnchorKind::Explicit,
                    },
                );
            }

            // Implicit section IDs
            let prefix = registry.resolve(path, "idprefix").unwrap_or("_");
            let separator = registry.resolve(path, "idseparator").unwrap_or("_");
            for section in &doc.sections {
                let id = generate_section_id(&section.title, prefix, separator);
                index.add_anchor(
                    &id,
                    AnchorLocation {
                        file: path.clone(),
                        line: section.line,
                        kind: AnchorKind::ImplicitSection,
                    },
                );
            }
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
        _source_file: &Path,
        _graph: &IncludeGraph,
        documents: &HashMap<PathBuf, DocumentModel>,
    ) -> Result<ResolvedXref, DiagnosticKind> {
        let (file_part, anchor_part) = parse_xref_target(&xref.target);

        if let Some(file_ref) = file_part {
            // xref:file.adoc#anchor[] or xref:file.adoc[]
            let target_file = documents.keys().find(|p| {
                p.ends_with(file_ref)
                    || p.file_name()
                        .is_some_and(|f| f.to_string_lossy() == file_ref)
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

    // --- Section ID generation ---

    #[test]
    fn test_section_id_basic() {
        assert_eq!(
            generate_section_id("Building Block View", "_", "_"),
            "_building_block_view"
        );
    }

    #[test]
    fn test_section_id_special_chars() {
        assert_eq!(
            generate_section_id("What is \"Clean Code\"?", "_", "_"),
            "_what_is_clean_code"
        );
    }

    #[test]
    fn test_section_id_custom_prefix() {
        assert_eq!(
            generate_section_id("Building Block View", "sec-", "-"),
            "sec-building-block-view"
        );
    }

    #[test]
    fn test_section_id_empty_prefix() {
        assert_eq!(
            generate_section_id("Building Block View", "", "_"),
            "building_block_view"
        );
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
            DocumentModel {
                path: PathBuf::from("/a.adoc"),
                sections: vec![],
                includes: vec![],
                xrefs: vec![],
                anchors: vec![
                    Anchor {
                        id: "a1".into(),
                        line: 1,
                    },
                    Anchor {
                        id: "a2".into(),
                        line: 5,
                    },
                ],
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
                anchors: vec![
                    Anchor {
                        id: "b1".into(),
                        line: 1,
                    },
                    Anchor {
                        id: "b2".into(),
                        line: 3,
                    },
                ],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![],
            },
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
            DocumentModel {
                path: PathBuf::from("/a.adoc"),
                sections: vec![Section {
                    level: 1,
                    title: "Building Block View".into(),
                    line: 10,
                }],
                includes: vec![],
                xrefs: vec![],
                anchors: vec![],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![],
            },
        );

        let index = ReferenceIndex::build(&documents, &registry);
        assert!(index.anchors.contains_key("_building_block_view"));
    }

    #[test]
    fn test_build_index_duplicate_anchor() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
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
                    id: "dup".into(),
                    line: 5,
                }],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![],
            },
        );

        let index = ReferenceIndex::build(&documents, &registry);
        // First wins, duplicate recorded
        assert_eq!(index.anchors.len(), 1);
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
            DocumentModel {
                path: PathBuf::from("/file.adoc"),
                sections: vec![],
                includes: vec![],
                xrefs: vec![],
                anchors: vec![Anchor {
                    id: "anchor".into(),
                    line: 42,
                }],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![],
            },
        );

        let index = ReferenceIndex::build(&documents, &registry);
        let graph = IncludeGraph::new();

        let xref = Xref {
            target: "file.adoc#anchor".into(),
            display_text: None,
            line: 1,
        };
        let result = index
            .resolve_xref(&xref, Path::new("/src.adoc"), &graph, &documents)
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
            DocumentModel {
                path: PathBuf::from("/doc.adoc"),
                sections: vec![Section {
                    level: 1,
                    title: "My Section".into(),
                    line: 5,
                }],
                includes: vec![],
                xrefs: vec![],
                anchors: vec![],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![],
            },
        );

        let index = ReferenceIndex::build(&documents, &registry);
        let graph = IncludeGraph::new();

        let xref = Xref {
            target: "_my_section".into(),
            display_text: None,
            line: 1,
        };
        let result = index
            .resolve_xref(&xref, Path::new("/other.adoc"), &graph, &documents)
            .unwrap();
        assert_eq!(result.file, PathBuf::from("/doc.adoc"));
        assert_eq!(result.line, 5);
    }

    #[test]
    fn test_resolve_xref_broken() {
        let index = ReferenceIndex::new();
        let graph = IncludeGraph::new();
        let documents = HashMap::new();

        let xref = Xref {
            target: "nonexistent-id".into(),
            display_text: None,
            line: 1,
        };
        let result = index.resolve_xref(&xref, Path::new("/src.adoc"), &graph, &documents);
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
            DocumentModel {
                path: PathBuf::from("/target.adoc"),
                sections: vec![],
                includes: vec![],
                xrefs: vec![],
                anchors: vec![],
                attributes: vec![],
                attribute_refs: vec![],
                conditionals: vec![],
            },
        );

        let index = ReferenceIndex::build(&documents, &registry);
        let graph = IncludeGraph::new();

        let xref = Xref {
            target: "target.adoc".into(),
            display_text: None,
            line: 1,
        };
        let result = index
            .resolve_xref(&xref, Path::new("/src.adoc"), &graph, &documents)
            .unwrap();
        assert_eq!(result.file, PathBuf::from("/target.adoc"));
        assert_eq!(result.line, 1);
    }
}
