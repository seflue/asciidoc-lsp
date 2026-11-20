use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::attributes::AttributeRegistry;
use crate::types::{DocumentModel, IncludeEdge, IncludeMacro, IncludeTree};

/// Value of a `leveloffset` setting as a number; unparsable values count as 0.
fn parse_offset(value: &str) -> i32 {
    value.trim().parse().unwrap_or(0)
}

/// The `leveloffset` after `value` is set while `current` is in force:
/// `+n`/`-n` add to it, a bare number replaces it.
pub(crate) fn next_leveloffset(current: i32, value: &str) -> i32 {
    if value.starts_with(['+', '-']) {
        current + parse_offset(value)
    } else {
        parse_offset(value)
    }
}

/// The `leveloffset` in force at `line` of `doc`, from its attribute entries.
/// `+n`/`-n` entries add to the current value, a bare number replaces it and
/// an unset entry resets it.
fn leveloffset_at(doc: &DocumentModel, line: usize) -> i32 {
    doc.attributes
        .iter()
        .filter(|def| def.line < line)
        .fold(0, |offset, def| match def.name.as_str() {
            "leveloffset" => next_leveloffset(offset, &def.value),
            "leveloffset!" | "!leveloffset" => 0,
            _ => offset,
        })
}

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

    /// For every document, the top-level documents whose include tree
    /// reaches it, sorted. A top-level document is one no other document
    /// includes, the one Asciidoctor converts; it reaches itself. A document
    /// reachable only through an include cycle has no entry.
    #[must_use]
    pub fn doc_roots(
        &self,
        documents: &HashMap<PathBuf, DocumentModel>,
    ) -> HashMap<PathBuf, Vec<PathBuf>> {
        let mut tops: Vec<&PathBuf> = documents
            .keys()
            .filter(|path| self.included_by(path).is_empty())
            .collect();
        tops.sort();
        let mut roots: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        for top in tops {
            let mut seen: HashSet<&Path> = HashSet::new();
            let mut stack = vec![top.as_path()];
            while let Some(file) = stack.pop() {
                if !seen.insert(file) || !documents.contains_key(file) {
                    continue;
                }
                roots
                    .entry(file.to_path_buf())
                    .or_default()
                    .push(top.clone());
                stack.extend(self.includes_of(file).iter().map(|e| e.target.as_path()));
            }
        }
        roots
    }

    /// Whether some includer shifts `file`'s headings with `leveloffset`,
    /// as an include attribute or as an attribute entry in force at the
    /// include line. A shifted file is a chapter, not the converted document.
    #[must_use]
    pub fn is_level_shifted(
        &self,
        file: &Path,
        documents: &HashMap<PathBuf, DocumentModel>,
    ) -> bool {
        self.included_by(file).iter().any(|parent| {
            self.includes_of(parent)
                .iter()
                .filter(|edge| edge.target == file)
                .any(|edge| {
                    edge.attributes
                        .iter()
                        .any(|(k, v)| k == "leveloffset" && parse_offset(v) != 0)
                        || documents
                            .get(parent)
                            .is_some_and(|doc| leveloffset_at(doc, edge.line) != 0)
                })
        })
    }

    /// Detect cycles in the include graph using DFS with 3-color marking.
    /// The closing include is the one that re-enters a file already open on
    /// the include stack, so the result depends on where the DFS starts.
    ///
    /// The DFS starts at each top-level document (nothing includes it) in
    /// path order, as Asciidoctor starts at the document it converts. The
    /// marks are shared, so every cycle is reported once, at the closing
    /// include of the first top-level document that reaches it; a second
    /// top-level document entering the same cycle elsewhere adds nothing.
    /// Files no top-level document reaches (cycles with no outside entry)
    /// follow in order of `rank` (lower first), then path order. `rank` is a
    /// heuristic for the converted document, for example a level-0 header
    /// that no include shifts; an untitled master inside a cycle gets the
    /// wrong closing line.
    /// Returns back-edges as `(source_file, include_line, target_file)`.
    #[must_use]
    pub fn detect_cycles(&self, rank: impl Fn(&Path) -> u8) -> Vec<(PathBuf, usize, PathBuf)> {
        let mut starts: Vec<&PathBuf> = self.forward.keys().collect();
        starts.sort_by_key(|path| (!self.included_by(path).is_empty(), rank(path), *path));

        let mut back_edges = Vec::new();
        let mut visiting = HashSet::new();
        let mut visited = HashSet::new();
        for start in starts {
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
            .map_or_default(|doc| (doc.sections.clone(), doc.xrefs.clone(), doc.anchors.clone()));

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
    // Like Asciidoctor's `File.absolute_path`: absolute, `..` popped, symlinks kept.
    let entry = std::path::absolute(entry).map_or_else(
        |_| entry.to_path_buf(),
        |path| crate::paths::normalize_lexically(&path),
    );

    let top = entry.canonicalize().unwrap_or_else(|_| entry.clone());
    walk(
        &entry,
        &top,
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

    // Inline parsing for xrefs and inline anchors; block anchors come from
    // the block tree. Parse each range individually to prevent cross-range
    // passthrough misparses.
    let inline_ranges = crate::extractors::collect_inline_ranges(root_node, source.as_bytes());
    let (xrefs, inline_anchors, inline_images) =
        crate::extractors::extract_inline_content_safe(&inline_ranges, &source);
    let mut images = crate::extractors::extract_block_images(root_node, source.as_bytes());
    images.extend(inline_images);
    images.sort_by_key(|image| image.line);
    let mut anchors = crate::extractors::extract_anchors(root_node, source.as_bytes());
    anchors.extend(inline_anchors);
    // Document order: the first definition of an id is the one that counts.
    anchors.sort_by_key(|anchor| anchor.line);
    let block_titles = crate::extractors::extract_block_titles(root_node, source.as_bytes());

    // Extract attribute references ({name} patterns) via byte-scanning
    let attribute_refs = crate::extractors::extract_attribute_refs(&source);

    // Extract conditional directives (ifdef/ifndef) via byte-scanning
    let conditionals = crate::extractors::extract_conditionals(&source);

    let tags = crate::extractors::extract_tag_directives(&source);

    let unterminated_blocks =
        crate::extractors::extract_unterminated_blocks(root_node, source.as_bytes());

    // Attribute references Asciidoctor substitutes, from the trees
    let (substituted_refs, possible_definitions) =
        crate::substituted_refs::extract(root_node, &source, &inline_ranges);

    // Ensure config chain is loaded for this file's document directory
    if let Some(dir) = registry.doc_dir(canonical).map(Path::to_path_buf) {
        registry.ensure_config_for_dir(&dir);
    }

    // Register file attributes
    registry.register_file_attributes(canonical, &file_attrs);

    let edges = include_edges(canonical, &includes_raw, registry);

    let doc = DocumentModel {
        path: canonical.to_path_buf(),
        sections,
        includes: includes_raw,
        xrefs,
        images,
        anchors,
        block_titles,
        attributes: file_attrs,
        attribute_refs,
        conditionals,
        substituted_refs,
        possible_definitions,
        tags,
        unterminated_blocks,
        line_count: source.lines().count(),
    };

    Some((doc, edges))
}

/// Resolve the include macros of `file` to graph edges, in the same order.
pub(crate) fn include_edges(
    file: &Path,
    includes: &[IncludeMacro],
    registry: &AttributeRegistry,
) -> Vec<IncludeEdge> {
    includes
        .iter()
        .map(|inc| {
            let target = crate::paths::resolve_doc_target(file, &inc.path, registry)
                .unwrap_or_else(|| PathBuf::from(&inc.path));
            IncludeEdge {
                target,
                line: inc.line,
                attributes: inc.attributes.clone(),
                placement: inc.placement,
            }
        })
        .collect()
}

fn walk(
    file: &Path,
    top: &Path,
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
    // `file` is the path as reached (entry or include path, symlinks kept):
    // its folder is the document directory. A file reached by several paths
    // is parsed once, so the first path in walk order sets it.
    if let Some(dir) = file.parent() {
        registry.set_doc_dir(&canonical, dir);
    }
    // Everything the walk reaches is converted as part of the entry, so an
    // included file resolves `{docdir}` before the graph is complete.
    if canonical != top {
        registry.assign_doc_root(&canonical, top);
    }

    let Some((doc, edges)) = parse_single_file(&canonical, registry, overrides) else {
        return;
    };

    // Add edges to graph and recurse into includes along the path as written.
    // `edges` is built from `doc.includes` in the same order.
    for (edge, include) in edges.iter().zip(&doc.includes) {
        graph.add_edge(&canonical, edge.clone());
        let next = crate::paths::resolve_doc_path(&canonical, &include.path, registry)
            .unwrap_or_else(|| edge.target.clone());
        walk(&next, top, registry, graph, documents, visited, overrides);
    }

    documents.insert(canonical, doc);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Placement;
    use std::fs;

    #[test]
    fn test_include_edges_keep_the_placement_of_the_macro() {
        let macros = vec![
            IncludeMacro {
                path: "a.adoc".into(),
                attributes: vec![],
                line: 1,
                target_col: 0,
                placement: Placement::Block,
            },
            IncludeMacro {
                path: "b.adoc".into(),
                attributes: vec![],
                line: 2,
                target_col: 0,
                placement: Placement::Body,
            },
        ];
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        let edges = include_edges(Path::new("/tmp/x.adoc"), &macros, &registry);
        assert_eq!(edges[0].placement, Placement::Block);
        assert_eq!(edges[1].placement, Placement::Body);
    }

    #[test]
    fn test_next_leveloffset_adds_relative_values_and_replaces_with_a_bare_number() {
        assert_eq!(next_leveloffset(1, "+1"), 2);
        assert_eq!(next_leveloffset(1, "-2"), -1);
        assert_eq!(next_leveloffset(1, "3"), 3);
        assert_eq!(next_leveloffset(1, ""), 0);
    }

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
                placement: Placement::Body,
            },
        );
        graph.add_edge(
            &a,
            IncludeEdge {
                target: c.clone(),
                line: 10,
                attributes: vec![],
                placement: Placement::Body,
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
                placement: Placement::Body,
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

    /// A file included through a symlinked folder resolves its own includes
    /// from the folder of the include path as written (the link), with `..`
    /// popped textually; the entry keeps its folder too if it is a symlink.
    #[cfg(unix)]
    #[test]
    fn test_graph_nested_include_through_symlinked_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let root = base.join("root");
        let elsewhere = base.join("elsewhere");
        fs::create_dir_all(elsewhere.join("dir")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(
            elsewhere.join("main.adoc"),
            "= A\n\ninclude::lnk/sub.adoc[]\n",
        )
        .unwrap();
        fs::write(
            elsewhere.join("dir/sub.adoc"),
            "== B\n\ninclude::../part.adoc[]\n",
        )
        .unwrap();
        fs::write(root.join("docs/part.adoc"), "Link side.\n").unwrap();
        fs::write(elsewhere.join("part.adoc"), "Target side.\n").unwrap();
        std::os::unix::fs::symlink(elsewhere.join("dir"), root.join("docs/lnk")).unwrap();
        let entry = root.join("docs/main.adoc");
        std::os::unix::fs::symlink(elsewhere.join("main.adoc"), &entry).unwrap();

        let mut registry = AttributeRegistry::new(&root);
        let (graph, _) = build_graph(&entry, &mut registry);

        let main_edges = graph.includes_of(&elsewhere.join("main.adoc"));
        assert_eq!(main_edges[0].target, elsewhere.join("dir/sub.adoc"));
        let sub_edges = graph.includes_of(&elsewhere.join("dir/sub.adoc"));
        assert_eq!(sub_edges[0].target, root.join("docs/part.adoc"));
    }

    /// For a file included through a folder link and through its real path,
    /// the path met first in walk order sets the document directory. `diamond.adoc` includes `guide/up.adoc` (through
    /// `guide -> ../outside/guide`) before `../outside/guide/up.adoc`.
    #[cfg(unix)]
    #[test]
    fn test_graph_first_include_path_sets_the_document_directory() {
        let fx = crate::symlink_fixture();
        let project = fx.join("real/project");
        let up = fx.join("real/outside/guide/up.adoc");
        let mut registry = AttributeRegistry::new(&project);

        let (graph, _) = build_graph(&project.join("diamond.adoc"), &mut registry);

        assert_eq!(registry.doc_dir(&up), Some(project.join("guide").as_path()));
        assert_eq!(
            graph.includes_of(&up)[0].target,
            project.join("part-up.adoc")
        );
    }

    /// The entry is taken as given, made absolute with `..` popped textually
    /// (Asciidoctor's `File.absolute_path`): `guide/../config/ch2.adoc` is the
    /// project's `config/ch2.adoc`, although `guide` links elsewhere.
    #[cfg(unix)]
    #[test]
    fn test_graph_entry_pops_parent_segments_textually() {
        let fx = crate::symlink_fixture();
        let project = fx.join("real/project");
        let ch2 = project.join("config/ch2.adoc");
        let mut registry = AttributeRegistry::new(&project);

        let (_, documents) = build_graph(&project.join("guide/../config/ch2.adoc"), &mut registry);

        assert!(documents.contains_key(&ch2), "{:?}", documents.keys());
        // The value of `config/.asciidoctorconfig`: the chain of `config/`.
        assert_eq!(registry.resolve(&ch2, "imagesdir"), Some("../assets"));
    }

    /// A relative entry resolves against the working directory (the crate
    /// folder under `cargo test`) and keeps the symlinked folder it names.
    #[cfg(unix)]
    #[test]
    fn test_graph_relative_entry_keeps_its_symlinked_folder() {
        let fx = crate::symlink_fixture();
        let project = fx.join("real/project");
        let up = fx.join("real/outside/guide/up.adoc");
        let entry = Path::new("../../tests/fixtures/symlinks/real/project/guide/up.adoc");
        assert_eq!(
            entry.canonicalize().ok(),
            Some(up.clone()),
            "precondition: the working directory is the crate folder"
        );
        let mut registry = AttributeRegistry::new(&project);

        let (graph, _) = build_graph(entry, &mut registry);

        assert_eq!(registry.doc_dir(&up), Some(project.join("guide").as_path()));
        assert_eq!(
            graph.includes_of(&up)[0].target,
            project.join("part-up.adoc")
        );
    }

    /// Block and inline anchors come from different trees but keep document
    /// order, so the first definition of an id is the one that counts.
    #[test]
    fn test_parse_single_file_keeps_anchors_in_document_order() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(
            &a,
            "Text with [[first]] inline.\n\n[[second]]\nMore text.\n",
        )
        .unwrap();
        let mut registry = AttributeRegistry::new(tmp.path());

        let (doc, _) = parse_single_file(&a, &mut registry, &HashMap::new()).unwrap();

        let anchors: Vec<_> = doc
            .anchors
            .iter()
            .map(|a| (a.id.as_str(), a.line))
            .collect();
        assert_eq!(anchors, vec![("first", 1), ("second", 3)]);
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
                placement: Placement::Body,
            },
        );
        graph.add_edge(
            &b,
            IncludeEdge {
                target: a.clone(),
                line: 5,
                attributes: vec![],
                placement: Placement::Body,
            },
        );

        let cycles = graph.detect_cycles(|_| 0);
        assert_eq!(cycles.len(), 1, "Expected 1 cycle edge, got: {cycles:?}");
        // No top-level document: the DFS starts at `a` (path order) and
        // `b -> a` closes the cycle.
        assert_eq!(cycles, vec![(b, 5, a)]);
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
                placement: Placement::Body,
            },
        );
        graph.add_edge(
            &b,
            IncludeEdge {
                target: c.clone(),
                line: 5,
                attributes: vec![],
                placement: Placement::Body,
            },
        );
        graph.add_edge(
            &c,
            IncludeEdge {
                target: a.clone(),
                line: 7,
                attributes: vec![],
                placement: Placement::Body,
            },
        );

        let cycles = graph.detect_cycles(|_| 0);
        assert_eq!(cycles.len(), 1, "Expected 1 cycle edge, got: {cycles:?}");
        // No top-level document: the DFS starts at `a` (path order) and
        // `c -> a` closes the cycle.
        assert_eq!(cycles, vec![(c, 7, a)]);
    }

    fn edge(target: &Path, line: usize) -> IncludeEdge {
        IncludeEdge {
            target: target.to_path_buf(),
            line,
            attributes: vec![],
            placement: Placement::Body,
        }
    }

    /// The untitled top-level `z` sorts after the cycle members and enters at
    /// `b`, so the closing edge is `a -> b`, not `b -> a`.
    #[test]
    fn test_detect_cycles_starts_at_top_level_document() {
        let mut graph = IncludeGraph::new();
        let (a, b, z) = (
            PathBuf::from("/a.adoc"),
            PathBuf::from("/b.adoc"),
            PathBuf::from("/z.adoc"),
        );
        graph.add_edge(&z, edge(&b, 1));
        graph.add_edge(&a, edge(&b, 3));
        graph.add_edge(&b, edge(&a, 5));

        assert_eq!(graph.detect_cycles(|_| 0), vec![(a, 3, b)]);
    }

    /// Two top-level documents enter the same cycle at different files; the
    /// cycle is reported once, at the closing include of the first one in
    /// path order (`m1 -> a -> b -> a`).
    #[test]
    fn test_detect_cycles_reports_one_closing_edge_per_cycle() {
        let mut graph = IncludeGraph::new();
        let (a, b, m1, m2) = (
            PathBuf::from("/a.adoc"),
            PathBuf::from("/b.adoc"),
            PathBuf::from("/m1.adoc"),
            PathBuf::from("/m2.adoc"),
        );
        graph.add_edge(&m1, edge(&a, 1));
        graph.add_edge(&m2, edge(&b, 1));
        graph.add_edge(&a, edge(&b, 3));
        graph.add_edge(&b, edge(&a, 3));

        assert_eq!(graph.detect_cycles(|_| 0), vec![(b, 3, a)]);
    }

    /// One top-level document enters the cycle at both members; still one edge.
    #[test]
    fn test_detect_cycles_single_top_entering_twice_reports_once() {
        let mut graph = IncludeGraph::new();
        let (a, b, top) = (
            PathBuf::from("/a.adoc"),
            PathBuf::from("/b.adoc"),
            PathBuf::from("/top.adoc"),
        );
        graph.add_edge(&top, edge(&a, 1));
        graph.add_edge(&top, edge(&b, 2));
        graph.add_edge(&a, edge(&b, 3));
        graph.add_edge(&b, edge(&a, 3));

        assert_eq!(graph.detect_cycles(|_| 0), vec![(b, 3, a)]);
    }

    /// Without an outside entry, the lower rank starts the DFS even when the
    /// path sorts later.
    #[test]
    fn test_detect_cycles_rank_picks_start_without_top_level_document() {
        let mut graph = IncludeGraph::new();
        let (a, z) = (PathBuf::from("/a.adoc"), PathBuf::from("/z.adoc"));
        graph.add_edge(&a, edge(&z, 3));
        graph.add_edge(&z, edge(&a, 5));

        let z_first = |p: &Path| u8::from(p != Path::new("/z.adoc"));
        assert_eq!(graph.detect_cycles(z_first), vec![(a, 3, z)]);
    }

    fn doc_with_entries(path: &str, entries: &[(&str, &str, usize)]) -> (PathBuf, DocumentModel) {
        let attrs = entries
            .iter()
            .map(|(name, value, line)| crate::types::AttributeDef {
                name: (*name).to_string(),
                value: (*value).to_string(),
                line: *line,
            })
            .collect();
        (
            PathBuf::from(path),
            DocumentModel::test(path).with_attributes(attrs),
        )
    }

    #[test]
    fn test_is_level_shifted_by_attribute_entry_or_include_attribute() {
        let (parent, doc) = doc_with_entries(
            "/index.adoc",
            &[("leveloffset", "+1", 3), ("leveloffset", "-1", 6)],
        );
        let documents = HashMap::from([(parent.clone(), doc)]);
        let mut graph = IncludeGraph::new();
        let (shifted, plain, inline) = (
            PathBuf::from("/shifted.adoc"),
            PathBuf::from("/plain.adoc"),
            PathBuf::from("/inline.adoc"),
        );
        graph.add_edge(&parent, edge(&shifted, 4));
        graph.add_edge(&parent, edge(&plain, 8));
        graph.add_edge(
            &parent,
            IncludeEdge {
                target: inline.clone(),
                line: 9,
                attributes: vec![("leveloffset".to_string(), "+1".to_string())],
                placement: Placement::Body,
            },
        );

        assert!(graph.is_level_shifted(&shifted, &documents));
        assert!(!graph.is_level_shifted(&plain, &documents), "reset by -1");
        assert!(graph.is_level_shifted(&inline, &documents));
        assert!(!graph.is_level_shifted(&parent, &documents));
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
                placement: Placement::Body,
            },
        );
        graph.add_edge(
            &b,
            IncludeEdge {
                target: c.clone(),
                line: 5,
                attributes: vec![],
                placement: Placement::Body,
            },
        );

        let cycles = graph.detect_cycles(|_| 0);
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
                placement: Placement::Body,
            },
        );

        let cycles = graph.detect_cycles(|_| 0);
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
