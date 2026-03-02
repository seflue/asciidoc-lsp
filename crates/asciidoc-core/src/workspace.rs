use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::attributes::AttributeRegistry;
use crate::diagnostics::collect_diagnostics;
use crate::graph::{build_graph, build_graph_with_overrides, IncludeGraph};
use crate::references::ReferenceIndex;
use crate::types::{Diagnostic, DocumentModel};

#[allow(dead_code)] // Fields are public API for LSP consumers
pub struct Workspace {
    pub root: PathBuf,
    pub attributes: AttributeRegistry,
    pub include_graph: IncludeGraph,
    pub reference_index: ReferenceIndex,
    pub documents: HashMap<PathBuf, DocumentModel>,
    pub diagnostics: Vec<Diagnostic>,
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspace")
            .field("root", &self.root)
            .field("documents", &self.documents.keys().collect::<Vec<_>>())
            .field("diagnostics_count", &self.diagnostics.len())
            .finish()
    }
}

impl Workspace {
    pub fn build(root: &Path, entry: &Path) -> Result<Self> {
        let mut registry = AttributeRegistry::new(root);
        // Load config from entry file's directory upward — .asciidoctorconfig
        // typically lives next to the main document, not at the workspace root.
        let config_start = entry.parent().unwrap_or(root);
        registry.load_config(config_start);
        let (graph, documents) = build_graph(entry, &mut registry);
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diagnostics = collect_diagnostics(&documents, &graph, &ref_index, &registry);
        Ok(Self {
            root: root.into(),
            attributes: registry,
            include_graph: graph,
            reference_index: ref_index,
            documents,
            diagnostics,
        })
    }

    /// Find all usages of an attribute `{name}` across the workspace.
    ///
    /// Returns a Vec of (file_path, line, start_col, end_col) for each reference.
    /// Only returns usages (references), not definitions.
    pub fn find_all_attribute_usages(&self, name: &str) -> Vec<(PathBuf, usize, usize, usize)> {
        let mut results = Vec::new();
        for doc in self.documents.values() {
            for attr_ref in &doc.attribute_refs {
                if attr_ref.name == name {
                    results.push((
                        doc.path.clone(),
                        attr_ref.line,
                        attr_ref.start_col,
                        attr_ref.end_col,
                    ));
                }
            }
        }
        results
    }

    /// Find all include statements that reference the given target file.
    ///
    /// Returns `(source_file, line, 0, 0)` for each `include::target[]` found.
    pub fn find_all_include_references(
        &self,
        target: &Path,
    ) -> Vec<(PathBuf, usize, usize, usize)> {
        let mut results = Vec::new();
        for (source_file, edges) in &self.include_graph.forward {
            for edge in edges {
                if edge.target == target {
                    results.push((source_file.clone(), edge.line, 0, 0));
                }
            }
        }
        results
    }

    /// Find all xrefs that reference the given filename (file part of xref target).
    ///
    /// Returns `(doc_path, line, 0, 0)` for each matching xref.
    pub fn find_all_file_xrefs(&self, filename: &str) -> Vec<(PathBuf, usize, usize, usize)> {
        let mut results = Vec::new();
        for doc in self.documents.values() {
            for xref in &doc.xrefs {
                let file_part = if let Some((file, _anchor)) = xref.target.split_once('#') {
                    if file.is_empty() {
                        continue;
                    }
                    file
                } else if xref.target.contains('.') {
                    xref.target.as_str()
                } else {
                    continue; // plain anchor, not a file reference
                };
                if file_part == filename
                    || file_part.ends_with(&format!("/{}", filename))
                    || filename.ends_with(&format!("/{}", file_part))
                {
                    results.push((doc.path.clone(), xref.line, 0, 0));
                }
            }
        }
        results
    }

    /// Find all xrefs that reference the given anchor ID.
    ///
    /// Matches xrefs with targets like `file.adoc#anchor`, `#anchor`, or `anchor`.
    /// Returns `(doc_path, line, 0, 0)` for each matching xref.
    pub fn find_all_anchor_references(
        &self,
        anchor_id: &str,
    ) -> Vec<(PathBuf, usize, usize, usize)> {
        let mut results = Vec::new();
        for doc in self.documents.values() {
            for xref in &doc.xrefs {
                let xref_anchor = if let Some((_file, anchor)) = xref.target.split_once('#') {
                    if anchor.is_empty() {
                        continue;
                    }
                    anchor
                } else if xref.target.contains('.') {
                    continue; // file-only reference, no anchor
                } else {
                    // Plain anchor ID (shorthand xref target)
                    xref.target.as_str()
                };
                if xref_anchor == anchor_id {
                    results.push((doc.path.clone(), xref.line, 0, 0));
                }
            }
        }
        results
    }

    /// Build workspace with content overrides for open editor buffers.
    ///
    /// Files present in `overrides` will be read from the map instead of disk.
    /// This allows the LSP to use in-memory content for files that the user
    /// has modified but not yet saved.
    pub fn build_with_overrides(
        root: &Path,
        entry: &Path,
        overrides: &HashMap<PathBuf, String>,
    ) -> Result<Self> {
        let mut registry = AttributeRegistry::new(root);
        let config_start = entry.parent().unwrap_or(root);
        registry.load_config(config_start);
        let (graph, documents) = build_graph_with_overrides(entry, &mut registry, overrides);
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diagnostics = collect_diagnostics(&documents, &graph, &ref_index, &registry);
        Ok(Self {
            root: root.into(),
            attributes: registry,
            include_graph: graph,
            reference_index: ref_index,
            documents,
            diagnostics,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_workspace_build_simple() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(&a, "= Doc A\n\ninclude::b.adoc[]\n").unwrap();
        fs::write(&b, "== Chapter B\n\nContent.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();

        assert_eq!(ws.documents.len(), 2);
        assert!(!ws.include_graph.forward.is_empty());
        // No broken refs expected
        let broken_xrefs: Vec<_> = ws
            .diagnostics
            .iter()
            .filter(|d| matches!(d.kind, crate::types::DiagnosticKind::BrokenXref { .. }))
            .collect();
        assert!(
            broken_xrefs.is_empty(),
            "Expected no broken xrefs, got: {:?}",
            broken_xrefs
        );
    }

    #[test]
    fn test_find_attribute_usages_across_files() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(".asciidoctorconfig"),
            ":imagesdir: images\n",
        )
        .unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");
        fs::write(
            &a,
            "= Doc A\n\nImages in {imagesdir} folder.\n\ninclude::b.adoc[]\n",
        )
        .unwrap();
        fs::write(&b, "== Chapter B\n\nAlso {imagesdir} here.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let usages = ws.find_all_attribute_usages("imagesdir");

        assert_eq!(usages.len(), 2, "Expected 2 usages, got: {:?}", usages);
    }

    #[test]
    fn test_find_attribute_usages_none() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Doc A\n:imagesdir: images\n\nNo refs here.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let usages = ws.find_all_attribute_usages("imagesdir");

        assert!(usages.is_empty(), "Expected no usages, got: {:?}", usages);
    }

    // --- adoc-0026 Task 1.2: find_all_include_references ---

    #[test]
    fn test_find_include_references_multi_file() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");
        let c = tmp.path().join("c.adoc");

        fs::write(&a, "= Doc A\n\ninclude::b.adoc[]\n\ninclude::c.adoc[]\n").unwrap();
        fs::write(&b, "== Chapter B\n\nContent.\n").unwrap();
        fs::write(&c, "== Chapter C\n\ninclude::b.adoc[]\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let b_canon = b.canonicalize().unwrap();
        let refs = ws.find_all_include_references(&b_canon);

        // b.adoc is included from a.adoc and c.adoc
        assert_eq!(refs.len(), 2, "Expected 2 include refs, got: {:?}", refs);
    }

    #[test]
    fn test_find_include_references_none() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(&a, "= Doc A\n\ninclude::b.adoc[]\n").unwrap();
        fs::write(&b, "== Chapter B\n\nContent.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        // a.adoc is not included by anyone
        let a_canon = a.canonicalize().unwrap();
        let refs = ws.find_all_include_references(&a_canon);

        assert!(refs.is_empty(), "Expected no include refs, got: {:?}", refs);
    }

    // --- adoc-0026 Task 1.3: find_all_file_xrefs ---

    #[test]
    fn test_find_file_xrefs_cross_doc() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(
            &a,
            "= Doc A\n\nSee xref:b.adoc#anchor[link text].\n\ninclude::b.adoc[]\n",
        )
        .unwrap();
        fs::write(&b, "[[anchor]]\n== Chapter B\n\nContent.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let refs = ws.find_all_file_xrefs("b.adoc");

        assert_eq!(refs.len(), 1, "Expected 1 file xref, got: {:?}", refs);
    }

    #[test]
    fn test_find_file_xrefs_multiple() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(
            &a,
            "= Doc A\n\nSee xref:b.adoc#a1[one] and xref:b.adoc#a2[two].\n\ninclude::b.adoc[]\n",
        )
        .unwrap();
        fs::write(
            &b,
            "[[a1]]\n== Chapter 1\n\n[[a2]]\n== Chapter 2\n\nContent.\n",
        )
        .unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let refs = ws.find_all_file_xrefs("b.adoc");

        assert_eq!(refs.len(), 2, "Expected 2 file xrefs, got: {:?}", refs);
    }

    // --- adoc-0026 Task 1.4: find_all_anchor_references ---

    #[test]
    fn test_find_anchor_refs_xref_macro() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(
            &a,
            "= Doc A\n\nSee xref:b.adoc#my-anchor[link].\n\ninclude::b.adoc[]\n",
        )
        .unwrap();
        fs::write(&b, "[[my-anchor]]\n== Chapter B\n\nContent.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let refs = ws.find_all_anchor_references("my-anchor");

        assert_eq!(refs.len(), 1, "Expected 1 anchor ref, got: {:?}", refs);
    }

    #[test]
    fn test_find_anchor_refs_shorthand() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(
            &a,
            "= Doc A\n\n[[my-anchor]]\n== Section A\n\ninclude::b.adoc[]\n",
        )
        .unwrap();
        fs::write(&b, "== Chapter B\n\nSee <<my-anchor>>.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let refs = ws.find_all_anchor_references("my-anchor");

        assert_eq!(refs.len(), 1, "Expected 1 anchor ref, got: {:?}", refs);
    }

    #[test]
    fn test_find_anchor_refs_cross_file() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");
        let c = tmp.path().join("c.adoc");

        fs::write(
            &a,
            "= Doc A\n\n[[target-id]]\n== Main\n\ninclude::b.adoc[]\n\ninclude::c.adoc[]\n",
        )
        .unwrap();
        fs::write(&b, "== Chapter B\n\nSee xref:a.adoc#target-id[link].\n").unwrap();
        fs::write(&c, "== Chapter C\n\nAlso <<target-id>>.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let refs = ws.find_all_anchor_references("target-id");

        assert_eq!(refs.len(), 2, "Expected 2 anchor refs, got: {:?}", refs);
    }

    #[test]
    fn test_find_anchor_refs_no_match() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");

        fs::write(&a, "= Doc A\n\n[[my-anchor]]\n== Section\n\nContent.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let refs = ws.find_all_anchor_references("nonexistent");

        assert!(refs.is_empty(), "Expected no anchor refs, got: {:?}", refs);
    }

    #[test]
    fn test_workspace_build_with_xref() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(
            &a,
            "= Doc A\n\n[[my-anchor]]\n== Section A\n\ninclude::b.adoc[]\n",
        )
        .unwrap();
        fs::write(&b, "== Chapter B\n\nSee <<my-anchor>>.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();

        assert_eq!(ws.documents.len(), 2);
        assert!(ws.reference_index.anchors.contains_key("my-anchor"));
    }
}
