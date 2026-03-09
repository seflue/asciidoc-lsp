use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::attributes::AttributeRegistry;
use crate::types::{DocumentModel, IncludeEdge, IncludeTree};

pub struct IncludeGraph {
    pub(crate) forward: HashMap<PathBuf, Vec<IncludeEdge>>,
    pub(crate) reverse: HashMap<PathBuf, Vec<PathBuf>>,
}

impl Default for IncludeGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl IncludeGraph {
    #[must_use]
    pub fn new() -> Self {
        Self {
            forward: HashMap::new(),
            reverse: HashMap::new(),
        }
    }

    pub fn add_edge(&mut self, from: &Path, edge: IncludeEdge) {
        let target = edge.target.clone();
        self.forward
            .entry(from.to_path_buf())
            .or_default()
            .push(edge);
        self.reverse
            .entry(target)
            .or_default()
            .push(from.to_path_buf());
    }

    /// Remove all forward edges originating from `file` and their reverse entries.
    pub fn remove_edges_from(&mut self, file: &Path) {
        if let Some(edges) = self.forward.remove(file) {
            for edge in &edges {
                if let Some(parents) = self.reverse.get_mut(&edge.target) {
                    parents.retain(|p| p != file);
                }
            }
        }
    }

    #[must_use]
    pub fn includes_of(&self, file: &Path) -> &[IncludeEdge] {
        static EMPTY: Vec<IncludeEdge> = Vec::new();
        self.forward.get(file).map_or(&EMPTY, |v| v.as_slice())
    }

    #[allow(dead_code)] // Public API for LSP consumers
    #[must_use]
    pub fn included_by(&self, file: &Path) -> &[PathBuf] {
        static EMPTY: Vec<PathBuf> = Vec::new();
        self.reverse.get(file).map_or(&EMPTY, |v| v.as_slice())
    }

    /// Detect cycles in the include graph using DFS with 3-color marking.
    /// Returns back-edges as `(source_file, include_line, target_file)`.
    #[must_use]
    pub fn detect_cycles(&self) -> Vec<(PathBuf, usize, PathBuf)> {
        let mut visiting = HashSet::new();
        let mut visited = HashSet::new();
        let mut back_edges = Vec::new();

        for start in self.forward.keys() {
            if !visited.contains(start) {
                self.dfs_cycle(start, &mut visiting, &mut visited, &mut back_edges);
            }
        }

        back_edges
    }

    fn dfs_cycle(
        &self,
        node: &Path,
        visiting: &mut HashSet<PathBuf>,
        visited: &mut HashSet<PathBuf>,
        back_edges: &mut Vec<(PathBuf, usize, PathBuf)>,
    ) {
        visiting.insert(node.to_path_buf());

        for edge in self.includes_of(node) {
            if visiting.contains(&edge.target) {
                back_edges.push((node.to_path_buf(), edge.line, edge.target.clone()));
            } else if !visited.contains(&edge.target) {
                self.dfs_cycle(&edge.target, visiting, visited, back_edges);
            }
        }

        visiting.remove(node);
        visited.insert(node.to_path_buf());
    }

    /// Generate an `IncludeTree` from the graph for CLI --tree output.
    /// Breaks cycles by tracking visited nodes.
    #[must_use]
    pub fn to_include_tree(
        &self,
        root: &Path,
        documents: &HashMap<PathBuf, DocumentModel>,
    ) -> IncludeTree {
        let mut visited = HashSet::new();
        self.build_tree_node(root, documents, &mut visited)
    }

    fn build_tree_node(
        &self,
        file: &Path,
        documents: &HashMap<PathBuf, DocumentModel>,
        visited: &mut HashSet<PathBuf>,
    ) -> IncludeTree {
        if !visited.insert(file.to_path_buf()) {
            return IncludeTree {
                file: file.to_path_buf(),
                includes: vec![],
                sections: vec![],
                xrefs: vec![],
                anchors: vec![],
                error: Some("Circular include detected".into()),
            };
        }

        let (sections, xrefs, anchors) = documents
            .get(file)
            .map(|doc| (doc.sections.clone(), doc.xrefs.clone(), doc.anchors.clone()))
            .unwrap_or_default();

        let children: Vec<IncludeTree> = self
            .includes_of(file)
            .iter()
            .map(|edge| self.build_tree_node(&edge.target, documents, visited))
            .collect();

        visited.remove(file);

        IncludeTree {
            file: file.to_path_buf(),
            includes: children,
            sections,
            xrefs,
            anchors,
            error: None,
        }
    }
}

/// Build the include graph by walking from the entry file.
/// Returns the graph and all parsed document models.
pub fn build_graph(
    entry: &Path,
    registry: &mut AttributeRegistry,
) -> (IncludeGraph, HashMap<PathBuf, DocumentModel>) {
    build_graph_with_overrides(entry, registry, &HashMap::new())
}

/// Build the include graph with content overrides for open editor buffers.
///
/// Files present in `overrides` will be read from the map instead of disk.
pub fn build_graph_with_overrides(
    entry: &Path,
    registry: &mut AttributeRegistry,
    overrides: &HashMap<PathBuf, Arc<String>>,
) -> (IncludeGraph, HashMap<PathBuf, DocumentModel>) {
    let mut graph = IncludeGraph::new();
    let mut documents = HashMap::new();
    let mut visited = HashSet::new();

    walk(
        entry,
        registry,
        &mut graph,
        &mut documents,
        &mut visited,
        overrides,
    );
    (graph, documents)
}

/// Parse a single file and return its `DocumentModel` plus resolved include edges.
///
/// This is the shared parse core used by both `walk()` (graph building) and
/// `Workspace::ensure_file()` (on-demand standalone parsing).
/// Returns `None` if the file cannot be read or parsed.
pub fn parse_single_file(
    canonical: &Path,
    registry: &mut AttributeRegistry,
    overrides: &HashMap<PathBuf, Arc<String>>,
) -> Option<(DocumentModel, Vec<IncludeEdge>)> {
    // Read file: prefer override content, fall back to disk
    let source: Cow<str> = match overrides.get(canonical) {
        Some(content) => Cow::Borrowed(content.as_str()),
        None => Cow::Owned(std::fs::read_to_string(canonical).ok()?),
    };

    // Parse with block parser
    let mut block_parser = crate::parser::init_block_parser().ok()?;
    let block_tree = crate::parser::parse(&mut block_parser, &source).ok()?;

    let root_node = block_tree.root_node();
    let sections = crate::extractors::extract_sections(root_node, source.as_bytes());
    let includes_raw = crate::extractors::extract_includes(root_node, source.as_bytes());
    let file_attrs = crate::extractors::extract_attributes(root_node, source.as_bytes());

    // Inline parsing for xrefs and anchors.
    // Parse each range individually to prevent cross-range passthrough
    // misparses (adoc-0023).
    let inline_ranges = crate::extractors::collect_inline_ranges(root_node);
    let (xrefs, anchors) = crate::extractors::extract_inline_content_safe(&inline_ranges, &source);

    // Extract attribute references ({name} patterns) via byte-scanning
    let attribute_refs = crate::extractors::extract_attribute_refs(&source);

    // Extract conditional directives (ifdef/ifndef) via byte-scanning
    let conditionals = crate::extractors::extract_conditionals(&source);

    // Ensure config chain is loaded for this file's directory
    if let Some(dir) = canonical.parent() {
        registry.ensure_config_for_dir(dir);
    }

    // Register file attributes
    registry.register_file_attributes(canonical, &file_attrs);

    // Resolve include paths to edges
    let base_dir = canonical.parent().unwrap_or(Path::new("."));
    let edges: Vec<IncludeEdge> = includes_raw
        .iter()
        .map(|inc| {
            let resolved_path = registry.substitute(canonical, &inc.path);
            let full_path = base_dir.join(&resolved_path);
            let target_canonical = full_path.canonicalize().unwrap_or(full_path);
            IncludeEdge {
                target: target_canonical,
                line: inc.line,
                attributes: inc.attributes.clone(),
            }
        })
        .collect();

    let doc = DocumentModel {
        path: canonical.to_path_buf(),
        sections,
        includes: includes_raw,
        xrefs,
        anchors,
        attributes: file_attrs,
        attribute_refs,
        conditionals,
    };

    Some((doc, edges))
}

fn walk(
    file: &Path,
    registry: &mut AttributeRegistry,
    graph: &mut IncludeGraph,
    documents: &mut HashMap<PathBuf, DocumentModel>,
    visited: &mut HashSet<PathBuf>,
    overrides: &HashMap<PathBuf, Arc<String>>,
) {
    let Ok(canonical) = file.canonicalize() else {
        return; // File doesn't exist — edge in graph is enough for diagnostics
    };

    if !visited.insert(canonical.clone()) {
        return; // Already parsed → dedup
    }

    let Some((doc, edges)) = parse_single_file(&canonical, registry, overrides) else {
        return;
    };

    // Add edges to graph and recurse into includes
    for edge in &edges {
        graph.add_edge(&canonical, edge.clone());
        walk(&edge.target, registry, graph, documents, visited, overrides);
    }

    documents.insert(canonical, doc);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // --- IncludeGraph unit tests ---

    #[test]
    fn test_add_and_query_edges() {
        let mut graph = IncludeGraph::new();
        let a = PathBuf::from("/a.adoc");
        let b = PathBuf::from("/b.adoc");
        let c = PathBuf::from("/c.adoc");

        graph.add_edge(
            &a,
            IncludeEdge {
                target: b.clone(),
                line: 5,
                attributes: vec![],
            },
        );
        graph.add_edge(
            &a,
            IncludeEdge {
                target: c.clone(),
                line: 10,
                attributes: vec![],
            },
        );

        let edges = graph.includes_of(&a);
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].target, b);
        assert_eq!(edges[1].target, c);
    }

    #[test]
    fn test_reverse_lookup() {
        let mut graph = IncludeGraph::new();
        let a = PathBuf::from("/a.adoc");
        let b = PathBuf::from("/b.adoc");

        graph.add_edge(
            &a,
            IncludeEdge {
                target: b.clone(),
                line: 5,
                attributes: vec![],
            },
        );

        let parents = graph.included_by(&b);
        assert_eq!(parents.len(), 1);
        assert_eq!(parents[0], a);
    }

    // --- Graph builder tests ---

    #[test]
    fn test_graph_builder_single_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("single.adoc");
        fs::write(&file, "= Title\n\nSome content.\n").unwrap();

        let mut registry = AttributeRegistry::new(tmp.path());
        let (graph, documents) = build_graph(&file, &mut registry);

        assert_eq!(documents.len(), 1);
        assert!(graph.forward.is_empty() || graph.forward.values().all(|v| v.is_empty()));
    }

    #[test]
    fn test_graph_builder_nested() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");
        let c = tmp.path().join("c.adoc");

        fs::write(&a, "= A\n\ninclude::b.adoc[]\n").unwrap();
        fs::write(&b, "== B\n\ninclude::c.adoc[]\n").unwrap();
        fs::write(&c, "=== C\n\nContent.\n").unwrap();

        let mut registry = AttributeRegistry::new(tmp.path());
        let (graph, documents) = build_graph(&a, &mut registry);

        assert_eq!(documents.len(), 3);

        let a_canon = a.canonicalize().unwrap();
        let b_canon = b.canonicalize().unwrap();
        let c_canon = c.canonicalize().unwrap();

        let a_edges = graph.includes_of(&a_canon);
        assert_eq!(a_edges.len(), 1);
        assert_eq!(a_edges[0].target, b_canon);

        let b_parents = graph.included_by(&b_canon);
        assert_eq!(b_parents.len(), 1);
        assert_eq!(b_parents[0], a_canon);

        let c_parents = graph.included_by(&c_canon);
        assert_eq!(c_parents.len(), 1);
        assert_eq!(c_parents[0], b_canon);
    }

    #[test]
    fn test_graph_builder_dedup() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");
        let c = tmp.path().join("c.adoc");

        fs::write(&a, "= A\n\ninclude::b.adoc[]\n\ninclude::c.adoc[]\n").unwrap();
        fs::write(&b, "== B\n\nContent.\n").unwrap();
        fs::write(&c, "== C\n\ninclude::b.adoc[]\n").unwrap();

        let mut registry = AttributeRegistry::new(tmp.path());
        let (_, documents) = build_graph(&a, &mut registry);

        // B is included from both A and C, but parsed only once
        assert_eq!(documents.len(), 3);
    }

    #[test]
    fn test_graph_builder_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(&a, "= A\n\ninclude::b.adoc[]\n").unwrap();
        fs::write(&b, "= B\n\ninclude::a.adoc[]\n").unwrap();

        let mut registry = AttributeRegistry::new(tmp.path());
        let (graph, documents) = build_graph(&a, &mut registry);

        // Both files parsed, no hang
        assert_eq!(documents.len(), 2);
        let a_canon = a.canonicalize().unwrap();
        assert!(graph.forward.contains_key(&a_canon));
    }

    #[test]
    fn test_graph_builder_missing_include() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= A\n\ninclude::nonexistent.adoc[]\n").unwrap();

        let mut registry = AttributeRegistry::new(tmp.path());
        let (graph, documents) = build_graph(&a, &mut registry);

        // a.adoc itself is parsed
        assert_eq!(documents.len(), 1);
        // Edge exists but target wasn't walked successfully
        let a_canon = a.canonicalize().unwrap();
        let edges = graph.includes_of(&a_canon);
        assert_eq!(edges.len(), 1);
    }

    // --- to_include_tree tests ---

    #[test]
    fn test_graph_to_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");
        let c = tmp.path().join("c.adoc");

        fs::write(&a, "= A\n\ninclude::b.adoc[]\n").unwrap();
        fs::write(&b, "== B\n\ninclude::c.adoc[]\n").unwrap();
        fs::write(&c, "=== C\n\nContent.\n").unwrap();

        let mut registry = AttributeRegistry::new(tmp.path());
        let (graph, documents) = build_graph(&a, &mut registry);

        let a_canon = a.canonicalize().unwrap();
        let tree = graph.to_include_tree(&a_canon, &documents);

        assert_eq!(tree.includes.len(), 1);
        assert_eq!(tree.includes[0].includes.len(), 1);
        assert!(tree.error.is_none());
    }

    #[test]
    fn test_graph_to_tree_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(&a, "= A\n\ninclude::b.adoc[]\n").unwrap();
        fs::write(&b, "= B\n\ninclude::a.adoc[]\n").unwrap();

        let mut registry = AttributeRegistry::new(tmp.path());
        let (graph, documents) = build_graph(&a, &mut registry);

        let a_canon = a.canonicalize().unwrap();
        let tree = graph.to_include_tree(&a_canon, &documents);

        // A → B → A(cycle)
        assert_eq!(tree.includes.len(), 1);
        let b_tree = &tree.includes[0];
        assert_eq!(b_tree.includes.len(), 1);
        assert!(b_tree.includes[0]
            .error
            .as_deref()
            .unwrap()
            .contains("Circular"));
    }

    // --- Cycle detection tests ---

    #[test]
    fn test_detect_cycles_direct() {
        let mut graph = IncludeGraph::new();
        let a = PathBuf::from("/a.adoc");
        let b = PathBuf::from("/b.adoc");

        graph.add_edge(
            &a,
            IncludeEdge {
                target: b.clone(),
                line: 3,
                attributes: vec![],
            },
        );
        graph.add_edge(
            &b,
            IncludeEdge {
                target: a.clone(),
                line: 5,
                attributes: vec![],
            },
        );

        let cycles = graph.detect_cycles();
        assert_eq!(cycles.len(), 1, "Expected 1 cycle edge, got: {cycles:?}");
        // Back-edge is either A→B or B→A depending on DFS start order
        let (src, _, tgt) = &cycles[0];
        assert!(
            (src == &a && tgt == &b) || (src == &b && tgt == &a),
            "Expected cycle between a and b, got: {cycles:?}"
        );
    }

    #[test]
    fn test_detect_cycles_indirect() {
        let mut graph = IncludeGraph::new();
        let a = PathBuf::from("/a.adoc");
        let b = PathBuf::from("/b.adoc");
        let c = PathBuf::from("/c.adoc");

        graph.add_edge(
            &a,
            IncludeEdge {
                target: b.clone(),
                line: 3,
                attributes: vec![],
            },
        );
        graph.add_edge(
            &b,
            IncludeEdge {
                target: c.clone(),
                line: 5,
                attributes: vec![],
            },
        );
        graph.add_edge(
            &c,
            IncludeEdge {
                target: a.clone(),
                line: 7,
                attributes: vec![],
            },
        );

        let cycles = graph.detect_cycles();
        assert_eq!(cycles.len(), 1, "Expected 1 cycle edge, got: {cycles:?}");
        // Back-edge closes the cycle — exact edge depends on DFS start order
        let (src, _, tgt) = &cycles[0];
        let nodes = [&a, &b, &c];
        assert!(
            nodes.contains(&src) && nodes.contains(&tgt),
            "Expected cycle among a/b/c, got: {cycles:?}"
        );
    }

    #[test]
    fn test_detect_cycles_none() {
        let mut graph = IncludeGraph::new();
        let a = PathBuf::from("/a.adoc");
        let b = PathBuf::from("/b.adoc");
        let c = PathBuf::from("/c.adoc");

        graph.add_edge(
            &a,
            IncludeEdge {
                target: b.clone(),
                line: 3,
                attributes: vec![],
            },
        );
        graph.add_edge(
            &b,
            IncludeEdge {
                target: c.clone(),
                line: 5,
                attributes: vec![],
            },
        );

        let cycles = graph.detect_cycles();
        assert!(cycles.is_empty(), "Expected no cycles, got: {cycles:?}");
    }

    #[test]
    fn test_detect_cycles_self() {
        let mut graph = IncludeGraph::new();
        let a = PathBuf::from("/a.adoc");

        graph.add_edge(
            &a,
            IncludeEdge {
                target: a.clone(),
                line: 2,
                attributes: vec![],
            },
        );

        let cycles = graph.detect_cycles();
        assert_eq!(cycles.len(), 1, "Expected 1 cycle edge, got: {cycles:?}");
        assert_eq!(cycles[0].0, a.clone());
        assert_eq!(cycles[0].1, 2);
        assert_eq!(cycles[0].2, a);
    }

    #[test]
    fn test_document_model_has_conditionals() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("doc.adoc");
        fs::write(
            &file,
            "= Title\n\nifdef::backend[]\nHTML output.\nendif::backend[]\n",
        )
        .unwrap();

        let mut registry = AttributeRegistry::new(tmp.path());
        let (_, documents) = build_graph(&file, &mut registry);

        let canon = file.canonicalize().unwrap();
        let doc = documents.get(&canon).unwrap();
        assert_eq!(
            doc.conditionals.len(),
            1,
            "Expected 1 conditional, got: {:?}",
            doc.conditionals
        );
        assert_eq!(doc.conditionals[0].attributes, vec!["backend"]);
    }

    #[test]
    fn test_parse_single_file_basic() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("single.adoc");
        fs::write(
            &file,
            "= Title\n:myattr: myvalue\n\nSome content with {myattr}.\n",
        )
        .unwrap();

        let canonical = file.canonicalize().unwrap();
        let mut registry = AttributeRegistry::new(tmp.path());
        let overrides = HashMap::new();

        let result = parse_single_file(&canonical, &mut registry, &overrides);
        assert!(result.is_some(), "parse_single_file should return Some");

        let (doc, edges) = result.unwrap();
        assert_eq!(doc.path, canonical);
        assert!(
            !doc.sections.is_empty(),
            "Should have at least a title section"
        );
        assert_eq!(doc.attributes.len(), 1, "Should have 1 attribute def");
        assert_eq!(doc.attributes[0].name, "myattr");
        assert_eq!(doc.attribute_refs.len(), 1, "Should have 1 attribute ref");
        assert_eq!(doc.attribute_refs[0].name, "myattr");
        assert!(edges.is_empty(), "No includes → no edges");
    }

    #[test]
    fn test_parse_single_file_with_includes() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");
        fs::write(&a, "= A\n\ninclude::b.adoc[]\n").unwrap();
        fs::write(&b, "== B\n\nContent.\n").unwrap();

        let canonical = a.canonicalize().unwrap();
        let mut registry = AttributeRegistry::new(tmp.path());
        let overrides = HashMap::new();

        let result = parse_single_file(&canonical, &mut registry, &overrides);
        assert!(result.is_some());

        let (doc, edges) = result.unwrap();
        assert_eq!(doc.includes.len(), 1, "Should have 1 include");
        assert_eq!(edges.len(), 1, "Should have 1 edge");
        let b_canon = b.canonicalize().unwrap();
        assert_eq!(edges[0].target, b_canon);
    }

    #[test]
    fn test_document_model_has_attribute_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("doc.adoc");
        fs::write(
            &file,
            "= Title\n:imagesdir: images\n\nSee {imagesdir} and {source-dir}.\n",
        )
        .unwrap();

        let mut registry = AttributeRegistry::new(tmp.path());
        let (_, documents) = build_graph(&file, &mut registry);

        let canon = file.canonicalize().unwrap();
        let doc = documents.get(&canon).unwrap();
        assert_eq!(
            doc.attribute_refs.len(),
            2,
            "Expected 2 attribute refs, got: {:?}",
            doc.attribute_refs
        );
        assert_eq!(doc.attribute_refs[0].name, "imagesdir");
        assert_eq!(doc.attribute_refs[1].name, "source-dir");
        assert_eq!(doc.attribute_refs[0].line, 4); // 1-based
    }
}
