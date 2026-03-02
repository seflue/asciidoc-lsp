use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::attributes::AttributeRegistry;
use crate::types::{DocumentModel, IncludeEdge, IncludeTree};

pub struct IncludeGraph {
    pub forward: HashMap<PathBuf, Vec<IncludeEdge>>,
    pub reverse: HashMap<PathBuf, Vec<PathBuf>>,
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
    overrides: &HashMap<PathBuf, String>,
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

fn walk(
    file: &Path,
    registry: &mut AttributeRegistry,
    graph: &mut IncludeGraph,
    documents: &mut HashMap<PathBuf, DocumentModel>,
    visited: &mut HashSet<PathBuf>,
    overrides: &HashMap<PathBuf, String>,
) {
    let Ok(canonical) = file.canonicalize() else {
        return; // File doesn't exist — edge in graph is enough for diagnostics
    };

    if !visited.insert(canonical.clone()) {
        return; // Already parsed → dedup
    }

    // Read file: prefer override content, fall back to disk (V-2)
    let source = match overrides.get(&canonical) {
        Some(content) => content.clone(),
        None => match std::fs::read_to_string(&canonical) {
            Ok(s) => s,
            Err(_) => return,
        },
    };

    // Parse with block parser
    let Ok(mut block_parser) = crate::parser::init_block_parser() else {
        return;
    };
    let Ok(block_tree) = crate::parser::parse(&mut block_parser, &source) else {
        return;
    };

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

    // Register file attributes
    registry.register_file_attributes(&canonical, &file_attrs);

    // Process includes: add edges and recurse (before storing DocumentModel so we can move includes_raw)
    let base_dir = canonical.parent().unwrap_or(Path::new("."));
    for inc in &includes_raw {
        let resolved_path = registry.substitute(&canonical, &inc.path);
        let full_path = base_dir.join(&resolved_path);
        let target_canonical = full_path.canonicalize().unwrap_or(full_path.clone());

        graph.add_edge(
            &canonical,
            IncludeEdge {
                target: target_canonical.clone(),
                line: inc.line,
                attributes: inc.attributes.clone(),
            },
        );

        walk(&full_path, registry, graph, documents, visited, overrides);
    }

    // Store DocumentModel (move includes_raw instead of clone)
    let doc = DocumentModel {
        path: canonical.clone(),
        sections,
        includes: includes_raw,
        xrefs,
        anchors,
        attributes: file_attrs,
        attribute_refs,
    };
    documents.insert(canonical.clone(), doc);
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
