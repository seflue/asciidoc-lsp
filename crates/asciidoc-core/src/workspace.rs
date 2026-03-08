use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::attributes::AttributeRegistry;
use crate::diagnostics::collect_diagnostics;
use crate::graph::{build_graph, build_graph_with_overrides, parse_single_file, IncludeGraph};
use crate::references::ReferenceIndex;
use crate::types::{Diagnostic, DocumentModel, FileLocation};

/// Recursively find all `.adoc` files under a directory.
/// Tracks visited canonical paths to avoid infinite loops from symlinks.
fn find_adoc_files(root: &Path) -> Vec<PathBuf> {
    let mut results = Vec::new();
    let mut visited = HashSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(canonical_dir) = dir.canonicalize() else {
            continue;
        };
        if !visited.insert(canonical_dir) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "adoc") {
                if let Ok(canonical) = path.canonicalize() {
                    results.push(canonical);
                }
            }
        }
    }
    results
}

#[allow(dead_code)] // Fields are public API for LSP consumers
pub struct Workspace {
    pub root: PathBuf,
    pub attributes: AttributeRegistry,
    pub include_graph: IncludeGraph,
    pub reference_index: ReferenceIndex,
    pub documents: HashMap<PathBuf, DocumentModel>,
    pub diagnostics: HashMap<PathBuf, Vec<Diagnostic>>,
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspace")
            .field("root", &self.root)
            .field("documents", &self.documents.keys().collect::<Vec<_>>())
            .field(
                "diagnostics_count",
                &self.diagnostics.values().map(Vec::len).sum::<usize>(),
            )
            .finish_non_exhaustive()
    }
}

impl Workspace {
    /// Create a new workspace with no documents or graph edges.
    ///
    /// Files are added later via `ensure_file()`. This is the primary
    /// constructor — no entry point or graph walk needed.
    #[must_use]
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.into(),
            attributes: AttributeRegistry::new(root),
            include_graph: IncludeGraph::new(),
            reference_index: ReferenceIndex::default(),
            documents: HashMap::new(),
            diagnostics: HashMap::new(),
        }
    }

    /// Build a workspace by parsing the entry file and all its includes.
    ///
    /// Convenience wrapper: creates a `new()` workspace, then populates it
    /// via graph walk from the entry file. Used by CLI and tests.
    /// Production LSP code uses `new()` + `ensure_file()` per opened file.
    #[must_use]
    pub fn build(root: &Path, entry: &Path) -> Self {
        let mut ws = Self::new(root);
        let (graph, documents) = build_graph(entry, &mut ws.attributes);
        ws.include_graph = graph;
        ws.documents = documents;
        ws.reference_index = ReferenceIndex::build(&ws.documents, &ws.attributes);
        ws.diagnostics = collect_diagnostics(
            &ws.documents,
            &ws.include_graph,
            &ws.reference_index,
            &ws.attributes,
        );
        ws
    }

    /// Find all usages of an attribute `{name}` across the workspace.
    ///
    /// Only returns usages (references), not definitions.
    #[must_use]
    pub fn find_all_attribute_usages(&self, name: &str) -> Vec<FileLocation> {
        let mut results = Vec::new();
        for doc in self.documents.values() {
            for attr_ref in &doc.attribute_refs {
                if attr_ref.name == name {
                    results.push(FileLocation::new(
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
    #[must_use]
    pub fn find_all_include_references(&self, target: &Path) -> Vec<FileLocation> {
        let mut results = Vec::new();
        for (source_file, edges) in &self.include_graph.forward {
            for edge in edges {
                if edge.target == target {
                    results.push(FileLocation::new(source_file.clone(), edge.line, 0, 0));
                }
            }
        }
        results
    }

    /// Find all xrefs that reference the given filename (file part of xref target).
    #[must_use]
    pub fn find_all_file_xrefs(&self, filename: &str) -> Vec<FileLocation> {
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
                    || file_part.ends_with(&format!("/{filename}"))
                    || filename.ends_with(&format!("/{file_part}"))
                {
                    results.push(FileLocation::new(doc.path.clone(), xref.line, 0, 0));
                }
            }
        }
        results
    }

    /// Find all xrefs that reference the given anchor ID.
    ///
    /// Matches xrefs with targets like `file.adoc#anchor`, `#anchor`, or `anchor`.
    #[must_use]
    pub fn find_all_anchor_references(&self, anchor_id: &str) -> Vec<FileLocation> {
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
                    results.push(FileLocation::new(doc.path.clone(), xref.line, 0, 0));
                }
            }
        }
        results
    }

    /// Ensure a file is present in the workspace, parsing it on-demand if needed.
    ///
    /// This is idempotent: if the file is already in `documents`, it returns immediately.
    /// Include edges are registered in the graph but NOT recursively walked.
    pub fn ensure_file(&mut self, file: &Path, overrides: &HashMap<PathBuf, String>) {
        if self.documents.contains_key(file) {
            return;
        }

        let Some((doc, edges)) = parse_single_file(file, &mut self.attributes, overrides) else {
            return;
        };

        for edge in &edges {
            self.include_graph.add_edge(file, edge.clone());
        }

        self.reference_index.add_file(file, &doc, &self.attributes);
        self.documents.insert(file.to_path_buf(), doc);
    }

    /// Get diagnostics for a specific file.
    #[must_use]
    pub fn diagnostics_for(&self, path: &Path) -> &[Diagnostic] {
        self.diagnostics.get(path).map_or(&[], |v| v.as_slice())
    }

    /// Get all diagnostics as a flat Vec (convenience for CLI and tests).
    #[must_use]
    pub fn all_diagnostics(&self) -> Vec<Diagnostic> {
        self.diagnostics.values().flatten().cloned().collect()
    }

    /// Recalculate diagnostics from current workspace state.
    ///
    /// Call this after `ensure_file()` or `update_file()` to refresh diagnostics.
    /// The reference index is already kept up-to-date incrementally by those methods.
    pub fn recalculate_diagnostics(&mut self) {
        self.diagnostics = collect_diagnostics(
            &self.documents,
            &self.include_graph,
            &self.reference_index,
            &self.attributes,
        );
    }

    /// Re-parse a single file that is already in the workspace.
    ///
    /// Removes old include edges for this file, re-parses with the given overrides,
    /// and updates `documents` and `include_graph`. Does NOT rebuild diagnostics
    /// — the caller is responsible for collecting diagnostics after the update.
    pub fn update_file(&mut self, file: &Path, overrides: &HashMap<PathBuf, String>) {
        // Remove old edges and index entries for this file
        self.include_graph.remove_edges_from(file);
        self.reference_index.remove_file(file);

        // Re-parse
        let Some((doc, edges)) = parse_single_file(file, &mut self.attributes, overrides) else {
            // File can't be parsed — remove from documents
            self.documents.remove(file);
            return;
        };

        for edge in &edges {
            self.include_graph.add_edge(file, edge.clone());
        }

        self.reference_index.add_file(file, &doc, &self.attributes);
        self.documents.insert(file.to_path_buf(), doc);
    }

    /// Scan the workspace root for all `.adoc` files and index them.
    ///
    /// This is a background scan: files are read from disk and added to the
    /// workspace via `ensure_file()`. Already-open files (from `did_open`) are
    /// not overwritten because `ensure_file()` is idempotent.
    pub fn scan_workspace(&mut self) {
        let overrides = HashMap::new();
        let files = find_adoc_files(&self.root);
        for file in &files {
            self.ensure_file(file, &overrides);
        }
    }

    /// Build workspace with content overrides for open editor buffers.
    ///
    /// Convenience wrapper: creates a `new()` workspace, then populates it
    /// via graph walk with override support. Used by tests.
    /// Production LSP code uses `new()` + `ensure_file()` per opened file.
    #[must_use]
    pub fn build_with_overrides(
        root: &Path,
        entry: &Path,
        overrides: &HashMap<PathBuf, String>,
    ) -> Self {
        let mut ws = Self::new(root);
        let (graph, documents) = build_graph_with_overrides(entry, &mut ws.attributes, overrides);
        ws.include_graph = graph;
        ws.documents = documents;
        ws.reference_index = ReferenceIndex::build(&ws.documents, &ws.attributes);
        ws.diagnostics = collect_diagnostics(
            &ws.documents,
            &ws.include_graph,
            &ws.reference_index,
            &ws.attributes,
        );
        ws
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_workspace_new_without_entry_point() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        // Workspace::new(root) creates an empty, functional workspace
        let mut ws = Workspace::new(&root);

        assert_eq!(ws.root, root);
        assert!(ws.documents.is_empty(), "New workspace has no documents");
        assert!(
            ws.include_graph.forward.is_empty(),
            "New workspace has no graph edges"
        );
        assert!(
            ws.diagnostics.is_empty(),
            "New workspace has no diagnostics"
        );

        // Files can be added later via ensure_file()
        let file = root.join("test.adoc");
        std::fs::write(&file, "= Test\n\nContent.\n").unwrap();
        let file_canon = file.canonicalize().unwrap();
        let overrides = HashMap::new();
        ws.ensure_file(&file_canon, &overrides);

        assert!(
            ws.documents.contains_key(&file_canon),
            "ensure_file() adds file to workspace"
        );
    }

    #[test]
    fn test_workspace_build_simple() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(&a, "= Doc A\n\ninclude::b.adoc[]\n").unwrap();
        fs::write(&b, "== Chapter B\n\nContent.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a);

        assert_eq!(ws.documents.len(), 2);
        assert!(!ws.include_graph.forward.is_empty());
        // No broken refs expected
        let broken_xrefs: Vec<_> = ws
            .all_diagnostics()
            .into_iter()
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

        let ws = Workspace::build(tmp.path(), &a);
        let usages = ws.find_all_attribute_usages("imagesdir");

        assert_eq!(usages.len(), 2, "Expected 2 usages, got: {:?}", usages);
    }

    #[test]
    fn test_find_attribute_usages_none() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Doc A\n:imagesdir: images\n\nNo refs here.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
        let refs = ws.find_all_anchor_references("target-id");

        assert_eq!(refs.len(), 2, "Expected 2 anchor refs, got: {:?}", refs);
    }

    #[test]
    fn test_find_anchor_refs_no_match() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");

        fs::write(&a, "= Doc A\n\n[[my-anchor]]\n== Section\n\nContent.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);

        assert_eq!(ws.documents.len(), 2);
        assert!(ws.reference_index.anchors.contains_key("my-anchor"));
    }

    // --- adoc-0067 Phase 3: Sibling config integration tests ---

    #[test]
    fn test_sibling_dirs_get_separate_configs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let docs = root.join("docs");
        let tests_dir = root.join("tests");
        fs::create_dir_all(&docs).unwrap();
        fs::create_dir_all(&tests_dir).unwrap();

        fs::write(root.join(".asciidoctorconfig"), ":root-attr: root-value\n").unwrap();
        fs::write(docs.join(".asciidoctorconfig"), ":docs-attr: docs-value\n").unwrap();
        fs::write(
            tests_dir.join(".asciidoctorconfig"),
            ":tests-attr: tests-value\n",
        )
        .unwrap();

        let main_adoc = docs.join("main.adoc");
        let test_adoc = tests_dir.join("test.adoc");
        fs::write(&main_adoc, "= Main Doc\n\ninclude::../tests/test.adoc[]\n").unwrap();
        fs::write(&test_adoc, "== Test Chapter\n\nContent.\n").unwrap();

        let ws = Workspace::build(&root, &main_adoc);

        let main_canon = main_adoc.canonicalize().unwrap();
        let test_canon = test_adoc.canonicalize().unwrap();

        // docs/main.adoc: has root-attr and docs-attr, but NOT tests-attr
        assert_eq!(
            ws.attributes.resolve(&main_canon, "root-attr"),
            Some("root-value")
        );
        assert_eq!(
            ws.attributes.resolve(&main_canon, "docs-attr"),
            Some("docs-value")
        );
        assert_eq!(ws.attributes.resolve(&main_canon, "tests-attr"), None);

        // tests/test.adoc: has root-attr and tests-attr, but NOT docs-attr
        assert_eq!(
            ws.attributes.resolve(&test_canon, "root-attr"),
            Some("root-value")
        );
        assert_eq!(
            ws.attributes.resolve(&test_canon, "tests-attr"),
            Some("tests-value")
        );
        assert_eq!(ws.attributes.resolve(&test_canon, "docs-attr"), None);
    }

    #[test]
    fn test_child_config_overrides_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();

        fs::write(
            root.join(".asciidoctorconfig"),
            ":shared-attr: root-value\n",
        )
        .unwrap();
        fs::write(sub.join(".asciidoctorconfig"), ":shared-attr: sub-value\n").unwrap();

        let entry = sub.join("file.adoc");
        fs::write(&entry, "= Sub Doc\n\nContent.\n").unwrap();

        let ws = Workspace::build(&root, &entry);
        let entry_canon = entry.canonicalize().unwrap();

        // Child config wins (last-wins merge)
        assert_eq!(
            ws.attributes.resolve(&entry_canon, "shared-attr"),
            Some("sub-value")
        );
    }

    // --- adoc-0069 Phase 2: ensure_file tests ---

    #[test]
    fn test_ensure_file_standalone() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        // Create entry + standalone file (not included by entry)
        let entry = root.join("index.adoc");
        let standalone = root.join("standalone.adoc");
        fs::write(&entry, "= Main\n\nContent.\n").unwrap();
        fs::write(
            &standalone,
            "= Standalone\n:myattr: val\n\nText with {myattr}.\n",
        )
        .unwrap();

        let mut ws = Workspace::build(&root, &entry);
        assert_eq!(ws.documents.len(), 1, "Only entry parsed initially");

        let standalone_canon = standalone.canonicalize().unwrap();
        let overrides = HashMap::new();
        ws.ensure_file(&standalone_canon, &overrides);

        assert_eq!(ws.documents.len(), 2, "Standalone file added");
        let doc = ws.documents.get(&standalone_canon).unwrap();
        assert_eq!(doc.attributes.len(), 1);
        assert_eq!(doc.attributes[0].name, "myattr");
    }

    #[test]
    fn test_ensure_file_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let entry = root.join("index.adoc");
        let standalone = root.join("standalone.adoc");
        fs::write(&entry, "= Main\n\nContent.\n").unwrap();
        fs::write(&standalone, "= Standalone\n\nContent.\n").unwrap();

        let mut ws = Workspace::build(&root, &entry);
        let standalone_canon = standalone.canonicalize().unwrap();
        let overrides = HashMap::new();

        ws.ensure_file(&standalone_canon, &overrides);
        assert_eq!(ws.documents.len(), 2);

        // Second call should not change anything
        ws.ensure_file(&standalone_canon, &overrides);
        assert_eq!(ws.documents.len(), 2, "Idempotent — no duplicate");
    }

    #[test]
    fn test_ensure_file_with_config() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let tests_dir = root.join("tests");
        fs::create_dir_all(&tests_dir).unwrap();

        fs::write(root.join(".asciidoctorconfig"), ":root-attr: root-val\n").unwrap();
        fs::write(
            tests_dir.join(".asciidoctorconfig"),
            ":test-attr: test-val\n",
        )
        .unwrap();

        let entry = root.join("index.adoc");
        fs::write(&entry, "= Main\n\nContent.\n").unwrap();

        let standalone = tests_dir.join("test.adoc");
        fs::write(&standalone, "= Test\n\nContent.\n").unwrap();

        let mut ws = Workspace::build(&root, &entry);
        let standalone_canon = standalone.canonicalize().unwrap();
        let overrides = HashMap::new();
        ws.ensure_file(&standalone_canon, &overrides);

        // Standalone file gets its directory's config chain
        assert_eq!(
            ws.attributes.resolve(&standalone_canon, "root-attr"),
            Some("root-val")
        );
        assert_eq!(
            ws.attributes.resolve(&standalone_canon, "test-attr"),
            Some("test-val")
        );
    }

    #[test]
    fn test_ensure_file_already_in_graph() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let entry = root.join("index.adoc");
        let included = root.join("chapter.adoc");
        fs::write(&entry, "= Main\n\ninclude::chapter.adoc[]\n").unwrap();
        fs::write(&included, "== Chapter\n\nContent.\n").unwrap();

        let mut ws = Workspace::build(&root, &entry);
        assert_eq!(ws.documents.len(), 2);

        let included_canon = included.canonicalize().unwrap();
        let overrides = HashMap::new();

        // File already in graph — ensure_file should be a no-op
        ws.ensure_file(&included_canon, &overrides);
        assert_eq!(ws.documents.len(), 2, "No duplicate entry");
    }

    #[test]
    fn test_asciidoctorconfigdir_per_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();

        fs::write(
            root.join(".asciidoctorconfig"),
            ":imagesdir: {asciidoctorconfigdir}/images\n",
        )
        .unwrap();
        fs::write(
            sub.join(".asciidoctorconfig"),
            ":sourcedir: {asciidoctorconfigdir}/src\n",
        )
        .unwrap();

        let entry = sub.join("file.adoc");
        fs::write(&entry, "= Sub Doc\n\nContent.\n").unwrap();

        let ws = Workspace::build(&root, &entry);
        let entry_canon = entry.canonicalize().unwrap();

        // imagesdir uses root's {asciidoctorconfigdir} → workspace/images
        assert_eq!(
            ws.attributes.resolve(&entry_canon, "imagesdir"),
            Some(format!("{}/images", root.display())).as_deref()
        );
        // sourcedir uses sub's {asciidoctorconfigdir} → workspace/sub/src
        assert_eq!(
            ws.attributes.resolve(&entry_canon, "sourcedir"),
            Some(format!("{}/src", sub.display())).as_deref()
        );
    }

    // --- adoc-0069: Additional coverage ---

    #[test]
    fn test_ensure_file_asciidoctorconfigdir_resolves() {
        // Standalone file in tests/ gets {asciidoctorconfigdir} from tests/.asciidoctorconfig,
        // NOT from docs/.asciidoctorconfig. This is the core bug scenario of adoc-0069.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let docs_dir = root.join("docs");
        let tests_dir = root.join("tests");
        fs::create_dir_all(&docs_dir).unwrap();
        fs::create_dir_all(&tests_dir).unwrap();

        fs::write(root.join(".asciidoctorconfig"), ":root-attr: root-val\n").unwrap();
        fs::write(
            docs_dir.join(".asciidoctorconfig"),
            ":docs-dir: {asciidoctorconfigdir}/assets\n",
        )
        .unwrap();
        fs::write(
            tests_dir.join(".asciidoctorconfig"),
            ":tests-dir: {asciidoctorconfigdir}/fixtures\n",
        )
        .unwrap();

        let entry = docs_dir.join("index.adoc");
        fs::write(&entry, "= Main\n\nContent.\n").unwrap();

        let standalone = tests_dir.join("standalone.adoc");
        fs::write(&standalone, "= Test\n\nContent.\n").unwrap();

        let mut ws = Workspace::build(&root, &entry);
        let standalone_canon = standalone.canonicalize().unwrap();
        let tests_dir_canon = tests_dir.canonicalize().unwrap();
        let overrides = HashMap::new();
        ws.ensure_file(&standalone_canon, &overrides);

        // {asciidoctorconfigdir} for tests/ must point to tests/, not docs/
        assert_eq!(
            ws.attributes.resolve(&standalone_canon, "tests-dir"),
            Some(format!("{}/fixtures", tests_dir_canon.display())).as_deref(),
            "tests-dir should resolve via tests/.asciidoctorconfig"
        );
        // docs-dir should NOT be visible to the standalone file in tests/
        assert_eq!(
            ws.attributes.resolve(&standalone_canon, "docs-dir"),
            None,
            "docs-dir must not leak into tests/ config chain"
        );
        // root-attr should be inherited
        assert_eq!(
            ws.attributes.resolve(&standalone_canon, "root-attr"),
            Some("root-val"),
            "root config must be inherited"
        );
    }

    #[test]
    fn test_empty_workspace_ensure_file() {
        // No entry point at all — Workspace::new(), then ensure_file() adds a file
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        fs::write(root.join(".asciidoctorconfig"), ":cfg-attr: cfg-val\n").unwrap();

        let standalone = root.join("note.adoc");
        fs::write(
            &standalone,
            "= My Note\n:doc-attr: doc-val\n\n== Section One\n\nContent.\n",
        )
        .unwrap();

        let mut ws = Workspace::new(&root);
        assert!(ws.documents.is_empty(), "Empty workspace has no documents");

        let standalone_canon = standalone.canonicalize().unwrap();
        let overrides = HashMap::new();
        ws.ensure_file(&standalone_canon, &overrides);

        // File was added
        assert_eq!(ws.documents.len(), 1, "File should be added");
        let doc = ws.documents.get(&standalone_canon).unwrap();

        // Sections parsed (document title "My Note" is level-0, "Section One" is level-1)
        assert!(doc.sections.len() >= 2, "Sections should be extracted");
        assert_eq!(doc.sections[0].title, "My Note");
        assert_eq!(doc.sections[1].title, "Section One");

        // Document-header attribute available
        assert_eq!(
            ws.attributes.resolve(&standalone_canon, "doc-attr"),
            Some("doc-val"),
            "Document-header attributes should be available"
        );

        // Config-chain attribute available
        assert_eq!(
            ws.attributes.resolve(&standalone_canon, "cfg-attr"),
            Some("cfg-val"),
            "Config-chain attributes should be available"
        );
    }

    #[test]
    fn test_ensure_file_includes_registered_not_walked() {
        // Standalone file has include::other.adoc[] — edge is registered in graph
        // but other.adoc is NOT in documents (not recursively walked).
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let entry = root.join("index.adoc");
        fs::write(&entry, "= Main\n\nContent.\n").unwrap();

        let standalone = root.join("standalone.adoc");
        fs::write(&standalone, "= Standalone\n\ninclude::other.adoc[]\n").unwrap();

        let other = root.join("other.adoc");
        fs::write(&other, "== Other\n\nOther content.\n").unwrap();

        let mut ws = Workspace::build(&root, &entry);
        assert_eq!(ws.documents.len(), 1);

        let standalone_canon = standalone.canonicalize().unwrap();
        let other_canon = other.canonicalize().unwrap();
        let overrides = HashMap::new();
        ws.ensure_file(&standalone_canon, &overrides);

        // Standalone was added
        assert_eq!(
            ws.documents.len(),
            2,
            "Only standalone added, not other.adoc"
        );
        assert!(
            ws.documents.contains_key(&standalone_canon),
            "standalone.adoc should be in documents"
        );
        assert!(
            !ws.documents.contains_key(&other_canon),
            "other.adoc must NOT be in documents (no recursive walk)"
        );

        // Include edge is registered in graph
        let edges = ws.include_graph.includes_of(&standalone_canon);
        assert_eq!(edges.len(), 1, "One include edge should be registered");
        assert_eq!(
            edges[0].target, other_canon,
            "Edge target should point to other.adoc"
        );
    }

    #[test]
    fn test_ensure_file_sibling_dirs_independent() {
        // Reproduces the original bug: docs/ has entry point, tests/ is standalone.
        // Both dirs have their own .asciidoctorconfig. After ensure_file() for
        // tests/test.adoc, it gets tests/ config, not docs/ config.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let docs_dir = root.join("docs");
        let tests_dir = root.join("tests");
        fs::create_dir_all(&docs_dir).unwrap();
        fs::create_dir_all(&tests_dir).unwrap();

        fs::write(root.join(".asciidoctorconfig"), ":root-attr: from-root\n").unwrap();
        fs::write(
            docs_dir.join(".asciidoctorconfig"),
            ":scope: docs\n:docs-only: yes\n",
        )
        .unwrap();
        fs::write(
            tests_dir.join(".asciidoctorconfig"),
            ":scope: tests\n:tests-only: yes\n",
        )
        .unwrap();

        let entry = docs_dir.join("index.adoc");
        fs::write(&entry, "= Docs\n\nContent.\n").unwrap();

        let test_file = tests_dir.join("test.adoc");
        fs::write(&test_file, "= Test\n\nContent.\n").unwrap();

        let mut ws = Workspace::build(&root, &entry);
        let test_canon = test_file.canonicalize().unwrap();
        let entry_canon = entry.canonicalize().unwrap();
        let overrides = HashMap::new();
        ws.ensure_file(&test_canon, &overrides);

        // docs/index.adoc sees docs/ config
        assert_eq!(
            ws.attributes.resolve(&entry_canon, "scope"),
            Some("docs"),
            "Entry file should see docs scope"
        );
        assert_eq!(
            ws.attributes.resolve(&entry_canon, "docs-only"),
            Some("yes")
        );
        assert_eq!(
            ws.attributes.resolve(&entry_canon, "tests-only"),
            None,
            "Entry file must not see tests-only"
        );

        // tests/test.adoc sees tests/ config
        assert_eq!(
            ws.attributes.resolve(&test_canon, "scope"),
            Some("tests"),
            "Standalone file should see tests scope"
        );
        assert_eq!(
            ws.attributes.resolve(&test_canon, "tests-only"),
            Some("yes")
        );
        assert_eq!(
            ws.attributes.resolve(&test_canon, "docs-only"),
            None,
            "Standalone file must not see docs-only"
        );

        // Both see root config
        assert_eq!(
            ws.attributes.resolve(&entry_canon, "root-attr"),
            Some("from-root")
        );
        assert_eq!(
            ws.attributes.resolve(&test_canon, "root-attr"),
            Some("from-root")
        );
    }

    // --- adoc-0072 Phase 4: Per-File-Diagnostics tests ---

    #[test]
    fn test_diagnostics_stored_per_file() {
        // Two files: A has a broken xref (warning), B has a broken include (error).
        // diagnostics_for() must return them separately.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let file_a = root.join("a.adoc");
        let file_b = root.join("b.adoc");

        // A has a broken xref
        fs::write(&file_a, "= Doc A\n\nSee <<nonexistent>>.\n").unwrap();
        // B has a broken include
        fs::write(&file_b, "= Doc B\n\ninclude::missing.adoc[]\n").unwrap();

        let mut ws = Workspace::new(&root);
        let overrides = HashMap::new();
        let file_a_canon = file_a.canonicalize().unwrap();
        let file_b_canon = file_b.canonicalize().unwrap();

        ws.ensure_file(&file_a_canon, &overrides);
        ws.ensure_file(&file_b_canon, &overrides);
        ws.recalculate_diagnostics();

        // Diagnostics for A: should contain the broken xref
        let diags_a = ws.diagnostics_for(&file_a_canon);
        assert!(
            diags_a
                .iter()
                .any(|d| matches!(&d.kind, crate::types::DiagnosticKind::BrokenXref { .. })),
            "File A should have a BrokenXref diagnostic, got: {:?}",
            diags_a
        );

        // Diagnostics for B: should contain the broken include
        let diags_b = ws.diagnostics_for(&file_b_canon);
        assert!(
            diags_b
                .iter()
                .any(|d| matches!(&d.kind, crate::types::DiagnosticKind::BrokenInclude { .. })),
            "File B should have a BrokenInclude diagnostic, got: {:?}",
            diags_b
        );

        // Lists are separate: A has no BrokenInclude, B has no BrokenXref
        assert!(
            !diags_a
                .iter()
                .any(|d| matches!(&d.kind, crate::types::DiagnosticKind::BrokenInclude { .. })),
            "File A must not have BrokenInclude"
        );
        assert!(
            !diags_b
                .iter()
                .any(|d| matches!(&d.kind, crate::types::DiagnosticKind::BrokenXref { .. })),
            "File B must not have BrokenXref"
        );
    }

    // --- adoc-0072 Phase 3: Per-File-Rebuild tests ---

    #[test]
    fn test_did_open_parses_only_opened_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let file_a = root.join("a.adoc");
        let file_b = root.join("b.adoc");
        fs::write(&file_a, "= Doc A\n\nContent A.\n").unwrap();
        fs::write(&file_b, "= Doc B\n\nContent B.\n").unwrap();

        let mut ws = Workspace::new(&root);
        let overrides = HashMap::new();
        let file_a_canon = file_a.canonicalize().unwrap();
        let file_b_canon = file_b.canonicalize().unwrap();

        // did_open for file A → only A in documents
        ws.ensure_file(&file_a_canon, &overrides);
        assert_eq!(ws.documents.len(), 1);
        assert!(ws.documents.contains_key(&file_a_canon));
        assert!(!ws.documents.contains_key(&file_b_canon));

        // did_open for file B → A and B in documents
        ws.ensure_file(&file_b_canon, &overrides);
        assert_eq!(ws.documents.len(), 2);
        assert!(ws.documents.contains_key(&file_a_canon));
        assert!(ws.documents.contains_key(&file_b_canon));
    }

    #[test]
    fn test_update_file_replaces_document_model() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let file_a = root.join("a.adoc");
        let file_b = root.join("b.adoc");
        fs::write(&file_a, "= Doc A\n\ninclude::b.adoc[]\n").unwrap();
        fs::write(&file_b, "== Chapter B\n\nContent.\n").unwrap();

        let mut ws = Workspace::new(&root);
        let file_a_canon = file_a.canonicalize().unwrap();
        let file_b_canon = file_b.canonicalize().unwrap();
        let overrides = HashMap::new();
        ws.ensure_file(&file_a_canon, &overrides);

        // A has include edge to B
        assert_eq!(ws.include_graph.includes_of(&file_a_canon).len(), 1);
        assert_eq!(
            ws.include_graph.includes_of(&file_a_canon)[0].target,
            file_b_canon
        );

        // Update A: remove the include
        let mut overrides_new = HashMap::new();
        overrides_new.insert(
            file_a_canon.clone(),
            "= Doc A Updated\n\nNo include anymore.\n".to_string(),
        );
        ws.update_file(&file_a_canon, &overrides_new);

        // A has new content, include edge removed
        let doc = ws.documents.get(&file_a_canon).unwrap();
        assert_eq!(doc.sections[0].title, "Doc A Updated");
        assert!(
            ws.include_graph.includes_of(&file_a_canon).is_empty(),
            "Include edge should be removed after update"
        );
    }

    #[test]
    fn test_did_change_reparses_only_changed_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let file_a = root.join("a.adoc");
        let file_b = root.join("b.adoc");
        fs::write(&file_a, "= Doc A\n\nContent A.\n").unwrap();
        fs::write(&file_b, "= Doc B\n\nContent B.\n").unwrap();

        let mut ws = Workspace::new(&root);
        let overrides = HashMap::new();
        let file_a_canon = file_a.canonicalize().unwrap();
        let file_b_canon = file_b.canonicalize().unwrap();

        ws.ensure_file(&file_a_canon, &overrides);
        ws.ensure_file(&file_b_canon, &overrides);

        // Remember B's original title
        let b_title_before = ws.documents.get(&file_b_canon).unwrap().sections[0]
            .title
            .clone();

        // did_change for A with new content
        let mut change_overrides = HashMap::new();
        change_overrides.insert(
            file_a_canon.clone(),
            "= Doc A Changed\n\nNew content.\n".to_string(),
        );
        ws.update_file(&file_a_canon, &change_overrides);

        // A has new content
        assert_eq!(
            ws.documents.get(&file_a_canon).unwrap().sections[0].title,
            "Doc A Changed"
        );
        // B is unchanged
        assert_eq!(
            ws.documents.get(&file_b_canon).unwrap().sections[0].title,
            b_title_before
        );
    }

    #[test]
    fn test_ensure_file_with_overrides() {
        // ensure_file() with override content (editor buffer) instead of disk content.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let entry = root.join("index.adoc");
        fs::write(&entry, "= Main\n\nContent.\n").unwrap();

        let standalone = root.join("buffered.adoc");
        // Disk content — should NOT be used
        fs::write(
            &standalone,
            "= Disk Version\n:disk-attr: disk-val\n\nDisk.\n",
        )
        .unwrap();

        let mut ws = Workspace::build(&root, &entry);
        let standalone_canon = standalone.canonicalize().unwrap();

        // Override: editor buffer has different content
        let mut overrides = HashMap::new();
        overrides.insert(
            standalone_canon.clone(),
            "= Buffer Version\n:buffer-attr: buffer-val\n\nBuffer.\n".to_string(),
        );
        ws.ensure_file(&standalone_canon, &overrides);

        // File was added
        assert!(ws.documents.contains_key(&standalone_canon));

        // Override content was used, not disk
        assert_eq!(
            ws.attributes.resolve(&standalone_canon, "buffer-attr"),
            Some("buffer-val"),
            "Buffer attribute should be present (override used)"
        );
        assert_eq!(
            ws.attributes.resolve(&standalone_canon, "disk-attr"),
            None,
            "Disk attribute must not be present (override should take precedence)"
        );
    }

    #[test]
    fn test_ensure_file_updates_reference_index_incrementally() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let file_a = root.join("a.adoc");
        let file_b = root.join("b.adoc");
        fs::write(&file_a, "[[anchor-a]]\n= Doc A\n").unwrap();
        fs::write(&file_b, "[[anchor-b]]\n= Doc B\n").unwrap();

        let mut ws = Workspace::new(&root);
        let overrides = HashMap::new();

        let a_canon = file_a.canonicalize().unwrap();
        ws.ensure_file(&a_canon, &overrides);

        // After adding file A, index should know anchor-a
        assert!(
            ws.reference_index.lookup("anchor-a").is_some(),
            "anchor-a should be in index after ensure_file"
        );

        let b_canon = file_b.canonicalize().unwrap();
        ws.ensure_file(&b_canon, &overrides);

        // After adding file B, index should know both
        assert!(ws.reference_index.lookup("anchor-a").is_some());
        assert!(
            ws.reference_index.lookup("anchor-b").is_some(),
            "anchor-b should be in index after ensure_file"
        );
    }

    #[test]
    fn test_update_file_updates_reference_index() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let file_a = root.join("a.adoc");
        fs::write(&file_a, "[[old-anchor]]\n= Doc A\n").unwrap();

        let mut ws = Workspace::new(&root);
        let a_canon = file_a.canonicalize().unwrap();

        let overrides = HashMap::new();
        ws.ensure_file(&a_canon, &overrides);
        assert!(ws.reference_index.lookup("old-anchor").is_some());

        // Update with new content that has a different anchor
        let mut overrides_new = HashMap::new();
        overrides_new.insert(
            a_canon.clone(),
            "[[new-anchor]]\n= Doc A Updated\n".to_string(),
        );
        ws.update_file(&a_canon, &overrides_new);

        assert!(
            ws.reference_index.lookup("old-anchor").is_none(),
            "old anchor should be removed after update"
        );
        assert!(
            ws.reference_index.lookup("new-anchor").is_some(),
            "new anchor should be in index after update"
        );
    }

    #[test]
    fn test_scan_workspace_indexes_all_adoc_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        // Create 3 .adoc files, one in a subdirectory
        fs::write(root.join("a.adoc"), "[[anchor-a]]\n= Doc A\n").unwrap();
        fs::write(root.join("b.adoc"), "= Doc B\n\nSee <<anchor-a>>.\n").unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/c.adoc"), "[[anchor-c]]\n= Doc C\n").unwrap();

        // Also create a non-.adoc file that should be ignored
        fs::write(root.join("readme.txt"), "Not an adoc file\n").unwrap();

        let mut ws = Workspace::new(&root);
        ws.scan_workspace();

        // All 3 .adoc files should be indexed
        assert_eq!(
            ws.documents.len(),
            3,
            "Expected 3 documents, got: {:?}",
            ws.documents.keys().collect::<Vec<_>>()
        );

        // Anchors from all files should be in the index
        assert!(
            ws.reference_index.lookup("anchor-a").is_some(),
            "anchor-a should be in index"
        );
        assert!(
            ws.reference_index.lookup("anchor-c").is_some(),
            "anchor-c should be in index"
        );
    }

    #[test]
    fn test_scan_workspace_gd_works_cross_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        fs::write(root.join("defs.adoc"), "[[my-target]]\n= Definitions\n").unwrap();
        fs::write(
            root.join("refs.adoc"),
            "= References\n\nSee xref:defs.adoc#my-target[link].\n",
        )
        .unwrap();

        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();

        // go-to-definition: my-target should resolve to defs.adoc
        let loc = ws.reference_index.lookup("my-target").unwrap();
        assert!(
            loc.file.ends_with("defs.adoc"),
            "my-target should be in defs.adoc, got: {:?}",
            loc.file
        );

        // No broken xref diagnostics
        let broken: Vec<_> = ws
            .all_diagnostics()
            .into_iter()
            .filter(|d| matches!(d.kind, crate::types::DiagnosticKind::BrokenXref { .. }))
            .collect();
        assert!(
            broken.is_empty(),
            "Expected no broken xrefs after scan, got: {:?}",
            broken
        );
    }

    #[test]
    fn test_did_open_overrides_background_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        // Disk content has anchor "disk-anchor"
        let file_a = root.join("a.adoc");
        fs::write(&file_a, "[[disk-anchor]]\n= Doc A\n").unwrap();

        let mut ws = Workspace::new(&root);
        ws.scan_workspace();

        let a_canon = file_a.canonicalize().unwrap();
        assert!(
            ws.reference_index.lookup("disk-anchor").is_some(),
            "disk-anchor should be indexed from scan"
        );

        // Simulate did_open with editor content that has a different anchor
        let mut overrides = HashMap::new();
        overrides.insert(
            a_canon.clone(),
            "[[editor-anchor]]\n= Doc A Edited\n".to_string(),
        );
        ws.update_file(&a_canon, &overrides);

        assert!(
            ws.reference_index.lookup("disk-anchor").is_none(),
            "disk-anchor should be gone after editor override"
        );
        assert!(
            ws.reference_index.lookup("editor-anchor").is_some(),
            "editor-anchor should be in index after override"
        );
    }

    #[test]
    fn test_did_close_reindexes_from_disk() {
        // Scenario: File opened with editor content, then closed.
        // After close, the file should be re-indexed from disk content,
        // NOT removed from the index entirely.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        // Disk content has anchor "disk-anchor"
        let file_a = root.join("a.adoc");
        fs::write(&file_a, "[[disk-anchor]]\n= Doc A\n").unwrap();

        // Another file references disk-anchor
        let file_b = root.join("b.adoc");
        fs::write(&file_b, "= Doc B\n\nSee xref:a.adoc#disk-anchor[link].\n").unwrap();

        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();

        let a_canon = file_a.canonicalize().unwrap();

        // Simulate did_open: override with editor content (different anchor)
        let mut overrides = HashMap::new();
        overrides.insert(
            a_canon.clone(),
            "[[editor-anchor]]\n= Doc A Edited\n".to_string(),
        );
        ws.update_file(&a_canon, &overrides);
        ws.recalculate_diagnostics();

        assert!(
            ws.reference_index.lookup("editor-anchor").is_some(),
            "editor-anchor should be in index while file is open"
        );
        assert!(
            ws.reference_index.lookup("disk-anchor").is_none(),
            "disk-anchor should be gone while editor has different content"
        );

        // Simulate did_close: re-index from disk (no overrides)
        ws.update_file(&a_canon, &HashMap::new());
        ws.recalculate_diagnostics();

        // After close, file should be re-indexed from disk content
        assert!(
            ws.documents.contains_key(&a_canon),
            "file should still be in workspace after close"
        );
        assert!(
            ws.reference_index.lookup("disk-anchor").is_some(),
            "disk-anchor should be back in index after re-indexing from disk"
        );
        assert!(
            ws.reference_index.lookup("editor-anchor").is_none(),
            "editor-anchor should be gone after re-indexing from disk"
        );
    }

    #[test]
    fn test_did_close_gd_still_works() {
        // Verify that go-to-definition still works for a closed file
        // because the file was re-indexed from disk, not removed.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        // Disk content has anchor "disk-target"
        let file_a = root.join("defs.adoc");
        fs::write(&file_a, "[[disk-target]]\n= Definitions\n").unwrap();

        let file_b = root.join("refs.adoc");
        fs::write(
            &file_b,
            "= References\n\nSee xref:defs.adoc#disk-target[link].\n",
        )
        .unwrap();

        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();

        let a_canon = file_a.canonicalize().unwrap();

        // Simulate did_open: override with editor content (different anchor)
        let mut overrides = HashMap::new();
        overrides.insert(
            a_canon.clone(),
            "[[editor-target]]\n= Definitions Edited\n".to_string(),
        );
        ws.update_file(&a_canon, &overrides);

        // Simulate did_close: re-index from disk
        ws.update_file(&a_canon, &HashMap::new());
        ws.recalculate_diagnostics();

        // gd on disk-target should still resolve to defs.adoc
        let lookup = ws.reference_index.lookup("disk-target");
        assert!(
            lookup.is_some(),
            "disk-target should still be resolvable after did_close re-index"
        );
        let loc = lookup.unwrap();
        assert!(
            loc.file.ends_with("defs.adoc"),
            "disk-target should resolve to defs.adoc, got: {:?}",
            loc.file
        );
    }

    #[test]
    fn test_find_adoc_files_symlink_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create a real .adoc file
        fs::write(root.join("real.adoc"), "= Real\n").unwrap();

        // Create a subdirectory with a symlink back to root → infinite loop
        let subdir = root.join("subdir");
        fs::create_dir(&subdir).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root, subdir.join("loop_back")).unwrap();

        // Must terminate and return the real file (not hang)
        let files = find_adoc_files(root);
        assert!(
            !files.is_empty(),
            "should find at least the real .adoc file"
        );
        // Should not contain duplicates from the loop
        let unique: std::collections::HashSet<_> = files.iter().collect();
        assert_eq!(
            files.len(),
            unique.len(),
            "no duplicate files from symlink loop"
        );
    }
}
