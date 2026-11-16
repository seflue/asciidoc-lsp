use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::attributes::{
    substitute_attributes, AttributeHit, AttributeLayer, AttributeRegistry, AttributeSource, DOCDIR,
};
use crate::diagnostics::collect_diagnostics;
use crate::graph::{
    build_graph, build_graph_with_overrides, include_edges, parse_single_file, IncludeGraph,
};
use crate::paths::relative_path;
use crate::pdf_theme::PdfResolution;
use crate::reading::{read_in_order, ReadingVisitor};
use crate::references::ReferenceIndex;
use crate::types::{AttributeDef, Diagnostic, DocumentModel, FileLocation};

/// Recursively find all `.adoc` files under a directory, as `(path as
/// reached, canonical path)` pairs in a stable order.
/// Tracks visited canonical paths to avoid infinite loops from symlinks.
fn find_adoc_files(root: &Path) -> Vec<(PathBuf, PathBuf)> {
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
        let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "adoc") {
                if let Ok(canonical) = path.canonicalize() {
                    results.push((path, canonical));
                }
            }
        }
    }
    results
}

pub struct Workspace {
    pub(crate) root: PathBuf,
    /// `root` canonicalized once; document paths are canonical.
    canonical_root: PathBuf,
    pub(crate) attributes: AttributeRegistry,
    pub(crate) include_graph: IncludeGraph,
    pub(crate) reference_index: ReferenceIndex,
    pub(crate) documents: HashMap<PathBuf, DocumentModel>,
    pub(crate) diagnostics: HashMap<PathBuf, Vec<Diagnostic>>,
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
    // --- Accessors ---

    /// Workspace root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Attribute registry for resolving document attributes.
    #[must_use]
    pub fn attributes(&self) -> &AttributeRegistry {
        &self.attributes
    }

    /// Include graph (forward and reverse edges).
    #[must_use]
    pub fn include_graph(&self) -> &IncludeGraph {
        &self.include_graph
    }

    /// Cross-reference / anchor index.
    #[must_use]
    pub fn reference_index(&self) -> &ReferenceIndex {
        &self.reference_index
    }

    /// Parsed document models keyed by canonical path.
    #[must_use]
    pub fn documents(&self) -> &HashMap<PathBuf, DocumentModel> {
        &self.documents
    }

    /// Diagnostics keyed by file path.
    #[must_use]
    pub fn diagnostics_map(&self) -> &HashMap<PathBuf, Vec<Diagnostic>> {
        &self.diagnostics
    }

    /// Construct a workspace with explicit field values.
    ///
    /// Used by tests in downstream crates that need custom
    /// `ReferenceIndex` or `AttributeRegistry` configurations.
    #[must_use]
    pub fn new_with(
        root: PathBuf,
        attributes: AttributeRegistry,
        include_graph: IncludeGraph,
        reference_index: ReferenceIndex,
        documents: HashMap<PathBuf, DocumentModel>,
        diagnostics: HashMap<PathBuf, Vec<Diagnostic>>,
    ) -> Self {
        Self {
            canonical_root: crate::paths::canonicalize_if_exists(root.clone()),
            root,
            attributes,
            include_graph,
            reference_index,
            documents,
            diagnostics,
        }
    }

    /// Create a new workspace with no documents or graph edges.
    ///
    /// Files are added later via `ensure_file()`. This is the primary
    /// constructor — no entry point or graph walk needed.
    #[must_use]
    pub fn new(root: &Path) -> Self {
        Self {
            canonical_root: crate::paths::canonicalize_if_exists(root.to_path_buf()),
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
        ws.reindex_all();
        ws.recalculate_diagnostics();
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

    /// Resolve `pdf-theme`, `pdf-themesdir` or `pdf-fontsdir` as seen from
    /// `file`: the entry on `line` when given, else the value a reference
    /// in `file` sees. See [`crate::pdf_theme::resolve_pdf_attribute`].
    #[must_use]
    pub fn resolve_pdf_attribute(
        &self,
        file: &Path,
        name: &str,
        line: Option<usize>,
    ) -> Option<PdfResolution> {
        crate::pdf_theme::resolve_pdf_attribute(
            &self.documents,
            &self.include_graph,
            &self.attributes,
            file,
            name,
            line,
        )
    }

    /// The attributes in effect on `line` of `file`, one scope per top-level
    /// document that reaches `file`, in path order. Asciidoctor converts each
    /// top-level document on its own, so each scope is correct for its
    /// document and none is preferred (ADR-4). A file no document includes
    /// is its own top-level document.
    #[must_use]
    pub fn scopes_at(&self, file: &Path, line: usize) -> Vec<AttributeScope<'_>> {
        let file = self.indexed_path(file);
        self.top_level_documents_of(&file)
            .unwrap_or_else(|| vec![file.clone()])
            .into_iter()
            .map(|root| self.scope(root, &file, line, false))
            .collect()
    }

    /// The attributes in effect on `line` of `file` when the editor previews
    /// `file` on its own: its own config chain, its own folder as `docdir`,
    /// nothing from the documents that include it. `None` for a top-level
    /// document, whose preview is its [`Self::scopes_at`] scope (ADR-4).
    #[must_use]
    pub fn preview_scope_at(&self, file: &Path, line: usize) -> Option<AttributeScope<'_>> {
        let file = self.indexed_path(file);
        self.top_level_documents_of(&file)?;
        Some(self.scope(file.clone(), &file, line, true))
    }

    /// What the entry for `name` in the config file `file` overrides: the
    /// entry loaded before it, else a builtin `scope` has for `name`. `None`
    /// when `file` is no config file or nothing is overridden.
    #[must_use]
    pub fn config_override(
        &self,
        file: &Path,
        scope: &AttributeScope<'_>,
        name: &str,
    ) -> Option<ConfigOverride> {
        let file = self.indexed_path(file);
        if let Some(entry) = self.attributes.overridden_config_attribute(&file, name) {
            let dir = self.attributes.doc_dir(&file)?;
            return Some(ConfigOverride::Entry {
                place: relative_path(dir, &entry.source.file),
                line: entry.source.line,
                raw: entry.raw,
            });
        }
        scope
            .definition(name)
            .filter(|definition| definition.origin == AttributeOrigin::Builtin)
            .map(|builtin| ConfigOverride::Builtin { raw: builtin.raw })
    }

    /// `name` before the first line of `document`: from its config chain,
    /// else built in. In a preview `docdir` is the document's own folder,
    /// not that of a document including it.
    fn preset(&self, document: &Path, name: &str, preview: bool) -> Option<AttributeDefinition> {
        let (layer, hit) = if preview && name == DOCDIR {
            let dir = self.attributes.doc_dir(document)?;
            (
                AttributeLayer::Builtin,
                AttributeHit {
                    value: dir.display().to_string(),
                    source: None,
                },
            )
        } else {
            self.attributes.preset(document, name)?
        };
        Some(AttributeDefinition {
            source: hit.source,
            origin: if layer == AttributeLayer::Config {
                AttributeOrigin::Config
            } else {
                AttributeOrigin::Builtin
            },
            raw: Some(hit.value.clone()),
            value: Some(hit.value),
        })
    }

    /// `file` as the workspace indexes it: as given, else canonical.
    #[must_use]
    pub fn indexed_path(&self, file: &Path) -> PathBuf {
        if self.documents.contains_key(file) {
            file.to_path_buf()
        } else {
            file.canonicalize().unwrap_or_else(|_| file.to_path_buf())
        }
    }

    /// The top-level documents that include `file`; `None` when there are
    /// none, so `file` is its own.
    fn top_level_documents_of(&self, file: &Path) -> Option<Vec<PathBuf>> {
        self.include_graph
            .doc_roots(&self.documents)
            .remove(file)
            .filter(|roots| !roots.is_empty() && roots.iter().any(|root| root != file))
    }

    /// The scope on `line` of `file`, reading the document `root`.
    fn scope(&self, root: PathBuf, file: &Path, line: usize, preview: bool) -> AttributeScope<'_> {
        let mut walk = ReadingWalk {
            workspace: self,
            reference: (file, line),
            preview,
            current: HashMap::new(),
            at_reference: None,
            definitions: Vec::new(),
        };
        read_in_order(self, &root, &mut walk);
        let (at_reference, before_reference) = walk.at_reference.unwrap_or_default();
        AttributeScope {
            workspace: self,
            document: root,
            preview,
            at_reference,
            before_reference,
            definitions: walk.definitions,
        }
    }

    /// Where `name`, referenced on `line` of `file`, is defined for
    /// navigation: the definition Asciidoctor uses in each top-level
    /// document (see [`Self::scopes_at`]) first, then the others in reading
    /// order, each site once.
    #[must_use]
    pub fn definition_sites(&self, file: &Path, line: usize, name: &str) -> Vec<AttributeSource> {
        let scopes = self.scopes_at(file, line);
        let effective: Vec<AttributeDefinition> = scopes
            .iter()
            .filter_map(|scope| scope.definition(name))
            .collect();
        let mut sites: Vec<AttributeSource> = Vec::new();
        let sources = effective
            .iter()
            .chain(scopes.iter().flat_map(|scope| scope.definitions(name)))
            .filter_map(|definition| definition.source.clone());
        for source in sources {
            if !sites.contains(&source) {
                sites.push(source);
            }
        }
        sites
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
    /// Each location spans the anchor id in the xref, as byte columns of its line.
    #[must_use]
    pub fn find_all_anchor_references(&self, anchor_id: &str) -> Vec<FileLocation> {
        let mut results = Vec::new();
        for doc in self.documents.values() {
            for xref in &doc.xrefs {
                let (xref_anchor, anchor_col) =
                    if let Some((file, anchor)) = xref.target.split_once('#') {
                        if anchor.is_empty() {
                            continue;
                        }
                        (anchor, xref.target_col + file.len() + 1)
                    } else if xref.target.contains('.') {
                        continue; // file-only reference, no anchor
                    } else {
                        // Plain anchor ID (shorthand xref target)
                        (xref.target.as_str(), xref.target_col)
                    };
                if xref_anchor == anchor_id {
                    results.push(FileLocation::new(
                        doc.path.clone(),
                        xref.line,
                        anchor_col,
                        anchor_col + anchor_id.len(),
                    ));
                }
            }
        }
        results
    }

    /// Ensure a file is present in the workspace, parsing it on-demand if needed.
    ///
    /// This is idempotent: if the file is already in `documents`, it returns immediately.
    /// Include edges are registered in the graph but NOT recursively walked.
    pub fn ensure_file(&mut self, file: &Path, overrides: &HashMap<PathBuf, Arc<String>>) {
        if self.documents.contains_key(file) {
            return;
        }

        let Some((doc, edges)) = parse_single_file(file, &mut self.attributes, overrides) else {
            return;
        };

        for edge in &edges {
            self.include_graph.add_edge(file, edge.clone());
        }

        self.documents.insert(file.to_path_buf(), doc);
        self.reindex_file(file);
        for edge in &edges {
            self.reindex_file(&edge.target);
        }
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

    /// Recompute each document's top-level documents, and so its `{docdir}`,
    /// from the include graph. A file whose `{docdir}` changed gets its
    /// include edges resolved again, which can change the graph, so this
    /// repeats until nothing changes (bounded).
    fn refresh_doc_roots(&mut self) {
        // Settles in two passes unless a `{docdir}` include adds or removes
        // an edge that changes another file's top-level documents.
        const MAX_PASSES: usize = 8;
        for _ in 0..MAX_PASSES {
            let roots = self.include_graph.doc_roots(&self.documents);
            let changed = self.attributes.set_doc_roots(&roots);
            if changed.is_empty() {
                return;
            }
            for file in changed {
                let Some(doc) = self.documents.get(&file) else {
                    continue;
                };
                let edges = include_edges(&file, &doc.includes, &self.attributes);
                self.include_graph.remove_edges_from(&file);
                for edge in edges {
                    self.include_graph.add_edge(&file, edge);
                }
            }
        }
    }

    /// Recalculate diagnostics from current workspace state.
    ///
    /// Call this after `ensure_file()` or `update_file()` to refresh diagnostics.
    /// Not a pure recomputation: it first parses and indexes include targets
    /// from disk that appeared since their includer was parsed (mutating
    /// `documents` and the reference index), then refreshes `{docdir}`, which
    /// depends on the whole include graph.
    pub fn recalculate_diagnostics(&mut self) {
        self.index_include_targets();
        self.refresh_doc_roots();
        self.diagnostics = collect_diagnostics(
            &self.canonical_root,
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
    pub fn update_file(&mut self, file: &Path, overrides: &HashMap<PathBuf, Arc<String>>) {
        // Remove old edges and index entries for this file
        let old_targets: Vec<PathBuf> = self
            .include_graph
            .includes_of(file)
            .iter()
            .map(|edge| edge.target.clone())
            .collect();
        self.include_graph.remove_edges_from(file);
        self.reference_index.remove_file(file);

        // Re-parse
        let Some((doc, edges)) = parse_single_file(file, &mut self.attributes, overrides) else {
            // File can't be parsed — remove from documents
            self.documents.remove(file);
            for target in &old_targets {
                self.reindex_file(target);
            }
            return;
        };

        for edge in &edges {
            self.include_graph.add_edge(file, edge.clone());
        }

        self.documents.insert(file.to_path_buf(), doc);
        self.reindex_file(file);
        for target in old_targets
            .iter()
            .chain(edges.iter().map(|edge| &edge.target))
        {
            self.reindex_file(target);
        }
    }

    /// Rebuild a file's reference index entries, including its header title
    /// ID when another document includes the file: only the header of a
    /// top-level document gets no ID.
    fn reindex_file(&mut self, file: &Path) {
        let Some(doc) = self.documents.get(file) else {
            return;
        };
        self.reference_index.remove_file(file);
        self.reference_index.add_file(file, doc, &self.attributes);
        if !self.include_graph.included_by(file).is_empty() {
            self.reference_index
                .add_header_title_id(file, doc, &self.attributes);
        }
    }

    fn reindex_all(&mut self) {
        let files: Vec<PathBuf> = self.documents.keys().cloned().collect();
        for file in &files {
            self.reindex_file(file);
        }
    }

    /// Remove a file from the workspace entirely.
    ///
    /// Drops include edges originating from the file, the reference index entries
    /// for its anchors, the parsed document model, any cached diagnostics, and the
    /// per-file attribute entry. Called when a `.adoc` file is deleted on disk.
    /// Caller is responsible for `recalculate_diagnostics()` afterwards.
    pub fn remove_file(&mut self, file: &Path) {
        let targets: Vec<PathBuf> = self
            .include_graph
            .includes_of(file)
            .iter()
            .map(|edge| edge.target.clone())
            .collect();
        self.include_graph.remove_edges_from(file);
        self.reference_index.remove_file(file);
        self.documents.remove(file);
        self.diagnostics.remove(file);
        self.attributes.forget_file(file);
        for target in &targets {
            self.reindex_file(target);
        }
    }

    /// Record the folder of the path `file` is addressed by (a symlink's
    /// folder for a symlinked document) as its document directory.
    ///
    /// Takes effect at the next parse of `file`; see
    /// [`AttributeRegistry::set_doc_dir`].
    pub fn set_doc_dir(&mut self, file: &Path, dir: &Path) {
        self.attributes.set_doc_dir(file, dir);
    }

    /// Re-read all `.asciidoctorconfig` chains and re-parse every indexed
    /// document against them.
    ///
    /// Called when an `.asciidoctorconfig*` file is created, changed or
    /// deleted on disk. Config attributes feed attribute resolution and
    /// include targets (`include::{dir}/x.adoc[]`), so every document is
    /// re-parsed, open ones from `overrides`. Caller is responsible for
    /// `recalculate_diagnostics()` afterwards.
    pub fn reload_config(&mut self, overrides: &HashMap<PathBuf, Arc<String>>) {
        self.attributes.clear_config_chains();
        let files: Vec<PathBuf> = self.documents.keys().cloned().collect();
        for file in files {
            self.update_file(&file, overrides);
        }
    }

    /// Scan the workspace root for all `.adoc` files and index them.
    ///
    /// This is a background scan: files are read from disk and added to the
    /// workspace via `ensure_file()`. Already-open files (from `did_open`) are
    /// not overwritten because `ensure_file()` is idempotent.
    ///
    /// A file whose canonical path lies outside the root, reached through a
    /// symlink, gets the link's folder as document directory (the first link
    /// in scan order). Files inside the root keep their canonical folder,
    /// whichever path the scan reaches them by.
    pub fn scan_workspace(&mut self) {
        let overrides = HashMap::new();
        let files = find_adoc_files(&self.root);
        let mut doc_dirs: HashMap<&Path, &Path> = HashMap::new();
        for (path, canonical) in &files {
            let dir = if canonical.starts_with(&self.canonical_root) {
                canonical.parent()
            } else {
                path.parent()
            };
            if let Some(dir) = dir {
                doc_dirs.entry(canonical).or_insert(dir);
            }
        }
        for (_, file) in &files {
            if let Some(dir) = doc_dirs.remove(file.as_path()) {
                if !self.documents.contains_key(file) {
                    self.attributes.set_doc_dir(file, dir);
                }
                self.ensure_file(file, &overrides);
            }
        }
        self.index_include_targets();
    }

    /// Index every `.adoc` file an indexed document includes that is on disk
    /// but not indexed yet, transitively.
    ///
    /// Includes created after their includer was parsed are the case this
    /// covers; open buffers are already indexed and stay untouched. Targets
    /// are keyed by their canonical path, like every other document; an
    /// includer whose edge held a non-canonical path gets its edges resolved
    /// again, so the graph points at the canonical key.
    fn index_include_targets(&mut self) {
        let overrides = HashMap::new();
        // Asciidoctor reads an include wherever it points, also outside the
        // root (`include::../shared/attributes.adoc[]`), so this walk does too.
        let mut stack: Vec<PathBuf> = self.documents.keys().cloned().collect();
        while let Some(file) = stack.pop() {
            let mut stale = false;
            let mut new_targets: Vec<(PathBuf, PathBuf)> = Vec::new();
            for edge in self.include_graph.includes_of(&file) {
                let target = &edge.target;
                if target.extension().is_none_or(|ext| ext != "adoc") || !target.is_file() {
                    continue;
                }
                let Ok(canonical) = target.canonicalize() else {
                    continue;
                };
                stale |= canonical != *target;
                if !self.documents.contains_key(&canonical)
                    && !new_targets.iter().any(|(c, _)| *c == canonical)
                {
                    new_targets.push((canonical, target.clone()));
                }
            }
            if stale {
                if let Some(doc) = self.documents.get(&file) {
                    let edges = include_edges(&file, &doc.includes, &self.attributes);
                    self.include_graph.remove_edges_from(&file);
                    for edge in edges {
                        self.include_graph.add_edge(&file, edge);
                    }
                }
            }
            for (canonical, addressed) in new_targets {
                if let Some(dir) = addressed.parent() {
                    self.attributes.set_doc_dir(&canonical, dir);
                }
                self.ensure_file(&canonical, &overrides);
                stack.push(canonical);
            }
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
        overrides: &HashMap<PathBuf, Arc<String>>,
    ) -> Self {
        let mut ws = Self::new(root);
        let (graph, documents) = build_graph_with_overrides(entry, &mut ws.attributes, overrides);
        ws.include_graph = graph;
        ws.documents = documents;
        ws.reindex_all();
        ws.recalculate_diagnostics();
        ws
    }
}

/// Where an [`AttributeDefinition`] comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributeOrigin {
    /// An attribute entry in a document of the include tree.
    Document,
    /// An `.asciidoctorconfig`, prepended to the top-level document.
    Config,
    /// Built in (`docdir`, `env-asciidoc-lsp`).
    Builtin,
}

/// What an entry of a config file overrides. From [`Workspace::config_override`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigOverride {
    /// An entry loaded before it, in a config file or one it includes.
    Entry {
        /// The file of the entry, relative to the document directory of the
        /// overriding config file.
        place: PathBuf,
        line: usize,
        /// The value as written; `None` for an unset.
        raw: Option<String>,
    },
    /// A builtin attribute.
    Builtin { raw: Option<String> },
}

/// One definition of an attribute, as Asciidoctor reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeDefinition {
    /// Where it is written; `None` for a builtin.
    pub source: Option<AttributeSource>,
    pub origin: AttributeOrigin,
    /// The value as written; `None` for an unset (`:name!:`, `:!name:`).
    pub raw: Option<String>,
    /// `raw` with the attribute values in effect at that point substituted.
    pub value: Option<String>,
}

/// The attributes in effect at one position of a file, as one top-level
/// document reads it. From [`Workspace::scopes_at`].
#[derive(Debug, Clone)]
pub struct AttributeScope<'a> {
    workspace: &'a Workspace,
    document: PathBuf,
    /// The editor's preview of a file other documents include
    /// ([`Workspace::preview_scope_at`]).
    preview: bool,
    /// The last definition of each name before the position (an unset too).
    at_reference: HashMap<String, AttributeDefinition>,
    /// How many entries of `definitions` come before the position.
    before_reference: usize,
    /// Every definition in reading order, with its name.
    definitions: Vec<(String, AttributeDefinition)>,
}

impl AttributeScope<'_> {
    /// The top-level document this scope belongs to; for a preview the
    /// previewed file.
    #[must_use]
    pub fn document(&self) -> &Path {
        &self.document
    }

    /// Whether this is the preview of a file other documents include.
    #[must_use]
    pub fn is_preview(&self) -> bool {
        self.preview
    }

    /// The definition of `name` Asciidoctor uses at the position: the last
    /// one before it (an unset too), else the config or a builtin, else the
    /// first one after it (lenient, so navigation still works when the
    /// order is wrong).
    #[must_use]
    pub fn definition(&self, name: &str) -> Option<AttributeDefinition> {
        self.at_reference
            .get(name)
            .cloned()
            .or_else(|| self.workspace.preset(&self.document, name, self.preview))
            .or_else(|| {
                self.definitions[self.before_reference..]
                    .iter()
                    .find(|(defined, _)| defined == name)
                    .map(|(_, definition)| definition.clone())
            })
    }

    /// The value of `name` Asciidoctor has at the position: from the last
    /// definition before it, else the config or a builtin. `None` when unset
    /// or unknown; unlike [`Self::definition`], a later definition does not
    /// count.
    #[must_use]
    pub fn value(&self, name: &str) -> Option<String> {
        value_of(&self.at_reference, name, || {
            self.workspace.preset(&self.document, name, self.preview)
        })
    }

    /// `input` with the values in effect at the position substituted; an
    /// unset or unknown reference stays as written.
    #[must_use]
    pub fn substitute(&self, input: &str) -> String {
        substitute_attributes(input, |name| self.value(name))
    }

    /// Every definition of `name` in the document, in reading order.
    pub fn definitions<'s>(
        &'s self,
        name: &'s str,
    ) -> impl Iterator<Item = &'s AttributeDefinition> + 's {
        self.definitions
            .iter()
            .filter(move |(defined, _)| defined == name)
            .map(|(_, definition)| definition)
    }

    /// Every attribute the scope knows, sorted by name, each with the
    /// definition [`Self::definition`] picks: those in the document and its
    /// includes (after the position too), the config chain and builtins.
    #[must_use]
    pub fn known(&self) -> Vec<(String, AttributeDefinition)> {
        let mut names: Vec<&str> = self
            .workspace
            .attributes
            .preset_names(&self.document)
            .into_iter()
            .chain(self.definitions.iter().map(|(name, _)| name.as_str()))
            .collect();
        if self.preview {
            names.push(DOCDIR);
        }
        names.sort_unstable();
        names.dedup();
        names
            .into_iter()
            .filter_map(|name| Some((name.to_string(), self.definition(name)?)))
            .collect()
    }
}

/// The value of `name` given the last definition of each name so far: its
/// value (`None` once unset), else that of `preset`.
fn value_of(
    current: &HashMap<String, AttributeDefinition>,
    name: &str,
    preset: impl FnOnce() -> Option<AttributeDefinition>,
) -> Option<String> {
    match current.get(name) {
        Some(definition) => definition.value.clone(),
        None => preset().and_then(|definition| definition.value),
    }
}

/// Reading-order walk of one top-level document for
/// [`Workspace::scopes_at`]. A file included twice is read twice.
struct ReadingWalk<'a> {
    workspace: &'a Workspace,
    reference: (&'a Path, usize),
    /// Reading a preview ([`Workspace::preview_scope_at`]).
    preview: bool,
    /// The last definition of each name so far; its `value` is `None` once
    /// unset.
    current: HashMap<String, AttributeDefinition>,
    /// `current` when the reference was reached, and how many definitions
    /// came before it.
    at_reference: Option<(HashMap<String, AttributeDefinition>, usize)>,
    definitions: Vec<(String, AttributeDefinition)>,
}

/// One line of a document in reading order. On the same line a reference
/// comes first: in `:a: {a}`, `{a}` means the earlier value.
enum WalkStep<'a> {
    Reference,
    Definition(&'a AttributeDef),
}

/// The attribute name of an entry, and whether the entry unsets it.
#[must_use]
pub fn entry_name(name: &str) -> (&str, bool) {
    name.strip_suffix('!')
        .or_else(|| name.strip_prefix('!'))
        .map_or((name, false), |name| (name, true))
}

impl<'a> ReadingVisitor<'a> for ReadingWalk<'a> {
    type Item = WalkStep<'a>;

    fn items(&self, file: &Path, doc: &'a DocumentModel) -> Vec<(usize, WalkStep<'a>)> {
        let mut items: Vec<(usize, WalkStep<'a>)> = doc
            .attributes
            .iter()
            .map(|attr| (attr.line, WalkStep::Definition(attr)))
            .collect();
        if self.reference.0 == file && self.at_reference.is_none() {
            items.push((self.reference.1, WalkStep::Reference));
        }
        items
    }

    fn first_on_line(item: &WalkStep<'a>) -> bool {
        matches!(item, WalkStep::Reference)
    }

    fn item(&mut self, stack: &[PathBuf], file: &Path, item: WalkStep<'a>) {
        match item {
            WalkStep::Reference => {
                if self.at_reference.is_none() {
                    self.at_reference = Some((self.current.clone(), self.definitions.len()));
                }
            }
            WalkStep::Definition(attr) => self.define(stack, file, attr),
        }
    }
}

impl ReadingWalk<'_> {
    fn define(&mut self, stack: &[PathBuf], file: &Path, attr: &AttributeDef) {
        let (name, unset) = entry_name(&attr.name);
        let definition = AttributeDefinition {
            source: Some(AttributeSource {
                file: file.to_path_buf(),
                line: attr.line,
            }),
            origin: AttributeOrigin::Document,
            raw: (!unset).then(|| attr.value.clone()),
            value: (!unset).then(|| self.substitute(stack, &attr.value)),
        };
        self.current.insert(name.to_string(), definition.clone());
        self.definitions.push((name.to_string(), definition));
    }

    /// `input` with the values in effect substituted; an unset or unknown
    /// reference stays as written.
    fn substitute(&self, stack: &[PathBuf], input: &str) -> String {
        substitute_attributes(input, |name| {
            value_of(&self.current, name, || {
                let root = stack.first()?;
                self.workspace.preset(root, name, self.preview)
            })
        })
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

    // --- find_all_include_references ---

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

    // --- find_all_file_xrefs ---

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

    // --- find_all_anchor_references ---

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

    // --- Sibling config integration tests ---

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

    // --- ensure_file tests ---

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

    // --- Additional coverage ---

    #[test]
    fn test_ensure_file_asciidoctorconfigdir_resolves() {
        // Standalone file in tests/ gets {asciidoctorconfigdir} from tests/.asciidoctorconfig,
        // NOT from docs/.asciidoctorconfig. This is the core bug scenario.
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

    // --- Per-File-Diagnostics tests ---

    // xrefs in default table cells take part in xref diagnostics.
    #[test]
    fn test_scan_workspace_checks_xrefs_in_table_cells() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::write(root.join("defs.adoc"), "[[my-target]]\n= Definitions\n").unwrap();
        fs::write(
            root.join("refs.adoc"),
            "= References\n\n|===\n| xref:defs.adoc#my-target[ok] | <<missing-target>>\n|===\n",
        )
        .unwrap();

        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();

        let broken: Vec<_> = ws
            .all_diagnostics()
            .into_iter()
            .filter_map(|d| match d.kind {
                crate::types::DiagnosticKind::BrokenXref { target } => Some(target),
                _ => None,
            })
            .collect();
        assert_eq!(broken, vec!["missing-target".to_string()]);
    }

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

    // --- Per-File-Rebuild tests ---

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
            Arc::new("= Doc A Updated\n\nNo include anymore.\n".to_string()),
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
            Arc::new("= Doc A Changed\n\nNew content.\n".to_string()),
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
            Arc::new("= Buffer Version\n:buffer-attr: buffer-val\n\nBuffer.\n".to_string()),
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
            Arc::new("[[new-anchor]]\n= Doc A Updated\n".to_string()),
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

    /// A scanned symlink resolves relative includes from its own folder, and
    /// one whose target lies outside the root keeps the root's config chain.
    #[cfg(unix)]
    #[test]
    fn test_scan_workspace_resolves_symlinked_files_from_link_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join(".asciidoctorconfig"), ":from-config: yes\n").unwrap();
        fs::write(root.join("docs/part.adoc"), "= Part\n").unwrap();
        let target = outside.join("doc.adoc");
        fs::write(&target, "= Doc\n\ninclude::part.adoc[]\n").unwrap();
        std::os::unix::fs::symlink(&target, root.join("docs/link.adoc")).unwrap();

        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();

        let diagnostics = ws.diagnostics_for(&target);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(ws.attributes.resolve(&target, "from-config"), Some("yes"));
    }

    /// A folder symlink pointing inside the root does not move the document
    /// directory of the files it reaches: they keep their canonical folder,
    /// whichever path the scan takes first.
    #[cfg(unix)]
    #[test]
    fn test_scan_workspace_keeps_canonical_folder_for_links_inside_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("common")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("docs/.asciidoctorconfig"), ":from-docs: yes\n").unwrap();
        let file = root.join("common/doc.adoc");
        fs::write(&file, "= Doc\n").unwrap();
        std::os::unix::fs::symlink("../common", root.join("docs/shared")).unwrap();

        let mut ws = Workspace::new(&root);
        ws.scan_workspace();

        assert_eq!(ws.attributes.resolve(&file, "from-docs"), None);
        assert_eq!(
            ws.attributes.doc_dir(&file),
            Some(root.join("common").as_path())
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
            Arc::new("[[editor-anchor]]\n= Doc A Edited\n".to_string()),
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
            Arc::new("[[editor-anchor]]\n= Doc A Edited\n".to_string()),
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
            Arc::new("[[editor-target]]\n= Definitions Edited\n".to_string()),
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

    /// Precondition of the LSP's option-A fixture test: the scan reaches
    /// `book/twice.adoc` through `config/book-link` first, so a scan that
    /// took the link's folder would show there.
    #[cfg(unix)]
    #[test]
    fn test_fixture_scan_reaches_book_through_the_link_first() {
        let project = crate::symlink_fixture().join("real/project");

        let files = find_adoc_files(&project);

        let reached: Vec<&PathBuf> = files
            .iter()
            .filter(|(_, canonical)| *canonical == project.join("book/twice.adoc"))
            .map(|(path, _)| path)
            .collect();
        assert_eq!(reached, vec![&project.join("config/book-link/twice.adoc")]);
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
        let unique: std::collections::HashSet<_> = files.iter().map(|(_, c)| c).collect();
        assert_eq!(
            files.len(),
            unique.len(),
            "no duplicate files from symlink loop"
        );
    }

    // --- remove_file + reload_config ---

    #[test]
    fn test_remove_file_clears_all_indexes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let a = root.join("a.adoc");
        let b = root.join("b.adoc");
        fs::write(
            &a,
            "[[anchor-a]]\n= Doc A\n\ninclude::b.adoc[]\n\nSee <<anchor-b>>.\n",
        )
        .unwrap();
        fs::write(&b, "[[anchor-b]]\n= Doc B\n").unwrap();

        let mut ws = Workspace::new(&root);
        let overrides = HashMap::new();
        let a_canon = a.canonicalize().unwrap();
        let b_canon = b.canonicalize().unwrap();
        ws.ensure_file(&a_canon, &overrides);
        ws.ensure_file(&b_canon, &overrides);
        ws.recalculate_diagnostics();

        // Pre-condition: both files indexed everywhere
        assert!(ws.documents.contains_key(&a_canon));
        assert!(ws.reference_index.lookup("anchor-a").is_some());
        assert_eq!(ws.include_graph.includes_of(&a_canon).len(), 1);

        // Remove file A
        ws.remove_file(&a_canon);

        assert!(!ws.documents.contains_key(&a_canon), "documents drops A");
        assert!(
            ws.reference_index.lookup("anchor-a").is_none(),
            "anchor index drops A's anchor"
        );
        assert!(
            ws.include_graph.includes_of(&a_canon).is_empty(),
            "include edges from A removed"
        );
        assert!(
            !ws.diagnostics.contains_key(&a_canon),
            "diagnostics for A removed"
        );

        // B must remain
        assert!(ws.documents.contains_key(&b_canon));
        assert!(ws.reference_index.lookup("anchor-b").is_some());
    }

    /// Workspace with `doc.adoc` (`{greeting}`) indexed from disk.
    fn workspace_with_doc(root: &Path) -> (Workspace, PathBuf) {
        let doc = root.join("doc.adoc");
        fs::write(&doc, "= Doc\n\n{greeting}\n").unwrap();
        let mut ws = Workspace::new(root);
        ws.ensure_file(&doc, &HashMap::new());
        (ws, doc)
    }

    #[test]
    fn test_reload_config_picks_up_changed_value() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::write(root.join(".asciidoctorconfig"), ":greeting: hello\n").unwrap();
        let (mut ws, doc) = workspace_with_doc(&root);
        assert_eq!(ws.attributes().resolve(&doc, "greeting"), Some("hello"));

        fs::write(root.join(".asciidoctorconfig"), ":greeting: bonjour\n").unwrap();
        ws.reload_config(&HashMap::new());

        assert_eq!(ws.attributes().resolve(&doc, "greeting"), Some("bonjour"));
    }

    #[test]
    fn test_reload_config_drops_value_of_deleted_config() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::write(root.join(".asciidoctorconfig"), ":greeting: hello\n").unwrap();
        let (mut ws, doc) = workspace_with_doc(&root);

        fs::remove_file(root.join(".asciidoctorconfig")).unwrap();
        ws.reload_config(&HashMap::new());

        assert_eq!(ws.attributes().resolve(&doc, "greeting"), None);
    }

    #[test]
    fn test_reload_config_reads_config_created_in_folder_without_one() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let sub = root.join("sub");
        fs::create_dir(&sub).unwrap();
        let doc = sub.join("doc.adoc");
        fs::write(&doc, "= Doc\n\n{greeting}\n").unwrap();
        let mut ws = Workspace::new(&root);
        ws.ensure_file(&doc, &HashMap::new());
        assert_eq!(ws.attributes().resolve(&doc, "greeting"), None);

        fs::write(sub.join(".asciidoctorconfig"), ":greeting: hello\n").unwrap();
        ws.reload_config(&HashMap::new());

        assert_eq!(ws.attributes().resolve(&doc, "greeting"), Some("hello"));
    }

    #[test]
    fn test_reload_config_follows_include_target_from_config() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for dir in ["one", "two"] {
            fs::create_dir(root.join(dir)).unwrap();
            fs::write(root.join(dir).join("part.adoc"), "Part\n").unwrap();
        }
        fs::write(root.join(".asciidoctorconfig"), ":partsdir: one\n").unwrap();
        let doc = root.join("doc.adoc");
        fs::write(&doc, "= Doc\n\ninclude::{partsdir}/part.adoc[]\n").unwrap();
        let mut ws = Workspace::new(&root);
        ws.ensure_file(&doc, &HashMap::new());

        fs::write(root.join(".asciidoctorconfig"), ":partsdir: two\n").unwrap();
        ws.reload_config(&HashMap::new());

        let targets: Vec<&Path> = ws
            .include_graph
            .includes_of(&doc)
            .iter()
            .map(|edge| edge.target.as_path())
            .collect();
        assert_eq!(targets, vec![root.join("two/part.adoc").as_path()]);
    }

    /// With both config files in the folder, a change to either one shows
    /// after reload.
    #[test]
    fn test_reload_config_with_both_config_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::write(root.join(".asciidoctorconfig"), ":greeting: hello\n").unwrap();
        fs::write(root.join(".asciidoctorconfig.adoc"), ":farewell: bye\n").unwrap();
        let (mut ws, doc) = workspace_with_doc(&root);
        assert_eq!(ws.attributes().resolve(&doc, "farewell"), Some("bye"));

        fs::write(root.join(".asciidoctorconfig.adoc"), ":farewell: ciao\n").unwrap();
        ws.reload_config(&HashMap::new());
        assert_eq!(ws.attributes().resolve(&doc, "farewell"), Some("ciao"));
        assert_eq!(ws.attributes().resolve(&doc, "greeting"), Some("hello"));

        fs::write(root.join(".asciidoctorconfig"), ":greeting: bonjour\n").unwrap();
        ws.reload_config(&HashMap::new());
        assert_eq!(ws.attributes().resolve(&doc, "greeting"), Some("bonjour"));
        assert_eq!(ws.attributes().resolve(&doc, "farewell"), Some("ciao"));
    }

    #[test]
    fn test_reload_config_keeps_open_buffer_content() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::write(root.join(".asciidoctorconfig"), ":greeting: hello\n").unwrap();
        let (mut ws, doc) = workspace_with_doc(&root);
        let buffer = Arc::new(String::from("= Doc\n:unsaved: yes\n\n{greeting}\n"));
        let overrides = HashMap::from([(doc.clone(), buffer)]);
        ws.update_file(&doc, &overrides);

        fs::write(root.join(".asciidoctorconfig"), ":greeting: bonjour\n").unwrap();
        ws.reload_config(&overrides);

        assert_eq!(ws.attributes().resolve(&doc, "unsaved"), Some("yes"));
        assert_eq!(ws.attributes().resolve(&doc, "greeting"), Some("bonjour"));
    }

    #[test]
    fn test_remove_file_idempotent_on_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let mut ws = Workspace::new(&root);

        // Should not panic on a path that is not in the workspace.
        ws.remove_file(&root.join("ghost.adoc"));
        assert!(ws.documents.is_empty());
    }

    /// An attribute entry in a section body, after a `:to` line without
    /// closing colon, is registered for the file (committed fixture
    /// `tests/fixtures/attribute_entries`).
    #[test]
    fn test_fixture_attribute_entry_in_section_body_is_registered() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/attribute_entries")
            .canonicalize()
            .unwrap();
        let main = root.join("main.adoc");

        let ws = Workspace::build(&root, &main);

        assert_eq!(ws.attributes().resolve(&main, "product"), Some("Widget"));
        assert_eq!(ws.attributes().resolve(&main, "to"), None);
    }

    fn docdir_fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/docdir_top_level")
            .canonicalize()
            .unwrap()
    }

    /// The include that closes a cycle is reported in the file that holds it.
    /// The traversal starts at the document Asciidoctor converts, so the edge
    /// that re-enters it is the closing one, whatever the path order.
    fn assert_circular_include_at(fixture: &str, file: &str, line: usize) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(fixture)
            .canonicalize()
            .unwrap();
        for _ in 0..20 {
            let mut ws = Workspace::new(&root);
            ws.scan_workspace();
            ws.recalculate_diagnostics();
            let mut circular: Vec<(PathBuf, usize)> = ws
                .all_diagnostics()
                .iter()
                .filter(|d| matches!(d.kind, crate::types::DiagnosticKind::CircularInclude { .. }))
                .map(|d| (d.file.clone(), d.line))
                .collect();
            circular.sort();
            assert_eq!(circular, vec![(root.join(file), line)], "{fixture}");
        }
    }

    #[test]
    fn test_fixture_circular_include_reported_at_closing_line() {
        assert_circular_include_at("circular_include_closing", "partials/legal.adoc", 6);
    }

    /// The master (`book.adoc`) sorts after the partial it includes.
    #[test]
    fn test_fixture_circular_include_master_sorts_last() {
        assert_circular_include_at("circular_include_master_last", "appendix/legal.adoc", 3);
    }

    /// The chapter has its own `= Title` but is shifted by `leveloffset`, so it
    /// is not the converted document, although its path sorts before `index.adoc`.
    #[test]
    fn test_fixture_circular_include_leveloffset_chapter_is_not_master() {
        assert_circular_include_at("circular_include_chapter_master", "chapters/intro.adoc", 3);
    }

    /// Same chapter, shifted with the attribute entry form
    /// (`:leveloffset: +1` before the include) instead of an include attribute.
    #[test]
    fn test_fixture_circular_include_leveloffset_attribute_entry_is_not_master() {
        assert_circular_include_at(
            "circular_include_chapter_master_attr",
            "chapters/intro.adoc",
            3,
        );
    }

    /// A book part file has a level-0 title as well, but only the master sets
    /// `:doctype:`; the part sorts first and must not win.
    #[test]
    fn test_fixture_circular_include_part_file_is_not_master() {
        assert_circular_include_at("circular_include_part_file", "a-part.adoc", 3);
    }

    /// The partial is included by the master and by a chapter.
    #[test]
    fn test_fixture_circular_include_partial_included_twice() {
        assert_circular_include_at("circular_include_shared_partial", "partials/legal.adoc", 3);
    }

    fn broken_includes(ws: &Workspace, file: &Path) -> usize {
        ws.diagnostics_for(file)
            .iter()
            .filter(|d| matches!(d.kind, crate::types::DiagnosticKind::BrokenInclude { .. }))
            .count()
    }

    /// `{docdir}` is the folder of the top-level document, also inside the
    /// files it includes; a file several top-level documents include sees
    /// the first one in sorted order, and lists all (committed fixture
    /// `tests/fixtures/docdir_top_level`).
    #[test]
    fn test_fixture_docdir_is_folder_of_top_level_document() {
        let root = docdir_fixture();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();
        let dir = |p: &str| root.join(p).display().to_string();
        let docdir = |p: &str| {
            ws.attributes()
                .resolve(&root.join(p), "docdir")
                .map(str::to_string)
        };

        assert_eq!(
            docdir("docs/handbook/handbook.adoc"),
            Some(dir("docs/handbook"))
        );
        assert_eq!(
            docdir("docs/print/guide/guide.adoc"),
            Some(dir("docs/print/guide"))
        );
        assert_eq!(
            docdir("docs/shared/pdf-settings.adoc"),
            Some(dir("docs/handbook"))
        );
        // Reached only through `include::{docdir}/...` in an included file.
        assert_eq!(
            docdir("docs/handbook/chapters/intro.adoc"),
            Some(dir("docs/handbook"))
        );
        assert_eq!(
            ws.attributes()
                .docdirs(&root.join("docs/shared/chapters.adoc")),
            vec![
                dir("docs/handbook").as_str(),
                dir("docs/print/guide").as_str()
            ]
        );
    }

    /// A `:docdir:` in `.asciidoctorconfig` has no effect: `docdir` stays the
    /// folder of the top-level document, so `include::{docdir}/...` resolves
    /// next to it (committed fixture `tests/fixtures/config_docdir_locked`,
    /// whose root config sets `:docdir: {asciidoctorconfigdir}` and then
    /// `:snippets: {docdir}/snippets`).
    #[test]
    fn test_fixture_config_docdir_is_ignored() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/config_docdir_locked")
            .canonicalize()
            .unwrap();
        let book = root.join("book/book.adoc");
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();

        let book_dir = root.join("book").display().to_string();
        assert_eq!(
            ws.attributes().resolve(&book, "docdir"),
            Some(book_dir.as_str())
        );
        assert!(ws.attributes().resolve_source(&book, "docdir").is_none());
        assert_eq!(broken_includes(&ws, &book), 0);
        // Later config lines do not see the ignored `:docdir:` either.
        assert_eq!(
            ws.attributes().resolve(&book, "snippets"),
            Some("{docdir}/snippets")
        );
    }

    /// A config file indexed as a document (`.asciidoctorconfig.adoc` is
    /// scanned as `*.adoc`) is not in its own config chain, as in IntelliJ.
    /// The other config file in the folder still applies, and
    /// `{asciidoctorconfigdir}` keeps its folder (committed fixture
    /// `tests/fixtures/config_both_files`).
    #[test]
    fn test_fixture_config_file_not_in_own_chain() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/config_both_files")
            .canonicalize()
            .unwrap();
        let config = root.join(".asciidoctorconfig.adoc");
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        assert!(ws.documents.contains_key(&config));
        let attrs = ws.attributes();

        assert!(attrs.preset(&config, "adoc-only").is_none());
        assert_eq!(
            attrs.resolve_source(&config, "adoc-only").map(|s| s.file),
            Some(config.clone())
        );
        let (layer, shared) = attrs.preset(&config, "shared").unwrap();
        assert_eq!(layer, AttributeLayer::Config);
        assert_eq!(shared.value, "plain");
        assert_eq!(attrs.resolve(&config, "shared"), Some("adoc"));
        let root_dir = root.display().to_string();
        assert_eq!(
            attrs.resolve(&config, "asciidoctorconfigdir"),
            Some(root_dir.as_str())
        );

        // Other documents in the folder still see the file.
        let doc = root.join("doc.adoc");
        assert_eq!(attrs.resolve(&doc, "adoc-only"), Some("from-adoc"));
    }

    /// `include::{docdir}/...` in an included file follows the top-level
    /// document's folder. The target is missing for the second including
    /// document only, so the include is not broken.
    #[test]
    fn test_fixture_docdir_include_from_included_file() {
        let root = docdir_fixture();
        let chapters = root.join("docs/shared/chapters.adoc");
        let intro = root.join("docs/handbook/chapters/intro.adoc");

        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();
        let targets: Vec<&Path> = ws
            .include_graph()
            .includes_of(&chapters)
            .iter()
            .map(|edge| edge.target.as_path())
            .collect();
        assert_eq!(targets, vec![intro.as_path()]);
        assert_eq!(broken_includes(&ws, &chapters), 0);

        // A graph walk from the entry gets there too.
        let ws = Workspace::build(&root, &root.join("docs/handbook/handbook.adoc"));
        assert!(ws.documents().contains_key(&intro));
        assert_eq!(broken_includes(&ws, &chapters), 0);
    }

    /// Every attribute a scope knows: definitions from the document and its
    /// includes, before and after the position, then the config chain and
    /// builtins, each name once with the definition the scope uses.
    #[test]
    fn test_scope_known_lists_included_config_and_builtin_attributes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::write(root.join(".asciidoctorconfig"), ":imagesdir: img\n").unwrap();
        fs::write(root.join("attrs.adoc"), ":product: Widget\n").unwrap();
        let main = root.join("main.adoc");
        fs::write(
            &main,
            "= Main\ninclude::attrs.adoc[]\n\nText.\n\n:late: later\n",
        )
        .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();

        let scopes = ws.scopes_at(&main, 4);
        let known = scopes[0].known();

        let value = |name: &str| {
            known
                .iter()
                .find(|(known_name, _)| known_name == name)
                .and_then(|(_, definition)| definition.value.clone())
        };
        assert_eq!(value("product").as_deref(), Some("Widget"));
        assert_eq!(value("late").as_deref(), Some("later"));
        assert_eq!(value("imagesdir").as_deref(), Some("img"));
        assert_eq!(value("docdir"), Some(root.display().to_string()));
        let names: Vec<&str> = known.iter().map(|(name, _)| name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names, sorted, "sorted by name, each once");
    }

    /// Definition sites list the definition in effect first, then the others
    /// in reading order, each once.
    #[test]
    fn test_definition_sites_lists_effective_first_then_the_rest_once() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let main = root.join("main.adoc");
        fs::write(&main, ":v: one\n\nText.\n\n:v: two\n").unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();

        let site = |line| AttributeSource {
            file: main.clone(),
            line,
        };
        assert_eq!(ws.definition_sites(&main, 3, "v"), vec![site(1), site(5)]);
        assert_eq!(ws.definition_sites(&main, 6, "v"), vec![site(5), site(1)]);
    }

    /// The scan follows includes out of the workspace root: Asciidoctor reads
    /// `include::../shared/attributes.adoc[]` wherever it lies, so its
    /// attribute definitions must be found too.
    #[test]
    fn test_fixture_attribute_defined_outside_root() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/attribute_outside_root")
            .canonicalize()
            .unwrap();
        let main = fixture.join("book/main.adoc");
        let shared = fixture.join("shared/attributes.adoc");

        let mut ws = Workspace::new(&fixture.join("book"));
        ws.scan_workspace();

        assert!(ws.documents().contains_key(&shared));
        let scopes = ws.scopes_at(&main, 4);
        assert_eq!(scopes.len(), 1);
        assert_eq!(
            scopes[0].definition("custom").unwrap().source,
            Some(AttributeSource {
                file: shared,
                line: 1
            })
        );
    }

    /// A config entry overrides the entry loaded before it, shown relative
    /// to its own folder; with none before it, a builtin; else nothing.
    #[test]
    fn test_config_override_entry_builtin_and_none() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(root.join(".asciidoctorconfig"), ":shared: root\n").unwrap();
        let config = sub.join(".asciidoctorconfig");
        fs::write(&config, ":shared: sub\n:env-asciidoc-lsp: x\n:fresh: 1\n").unwrap();
        let doc = sub.join("doc.adoc");
        fs::write(&doc, "= Doc\n").unwrap();

        let mut ws = Workspace::new(&root);
        ws.ensure_file(&config, &HashMap::new());
        ws.ensure_file(&doc, &HashMap::new());
        let config_override = |file: &Path, line: usize, name: &str| {
            let scopes = ws.scopes_at(file, line);
            ws.config_override(file, &scopes[0], name)
        };

        assert_eq!(
            config_override(&config, 0, "shared"),
            Some(ConfigOverride::Entry {
                place: PathBuf::from("../.asciidoctorconfig"),
                line: 1,
                raw: Some("root".to_string()),
            })
        );
        assert_eq!(
            config_override(&config, 1, "env-asciidoc-lsp"),
            Some(ConfigOverride::Builtin {
                raw: Some(String::new())
            })
        );
        assert_eq!(config_override(&config, 2, "fresh"), None);
        assert_eq!(config_override(&doc, 0, "shared"), None);
    }

    /// Every context in `tests/fixtures/xref_contexts/main.adoc` holds one
    /// broken xref; Asciidoctor reports exactly these targets; the `not-reported-*` lines (raw blocks and
    /// comments nested in quotes) stay quiet.
    #[test]
    fn test_fixture_xref_contexts_report_broken_xrefs() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/xref_contexts")
            .canonicalize()
            .unwrap();
        let main = root.join("main.adoc");

        let ws = Workspace::build(&root, &main);

        let mut broken: Vec<(usize, String)> = ws
            .diagnostics_for(&main)
            .iter()
            .filter_map(|d| match &d.kind {
                crate::types::DiagnosticKind::BrokenXref { target } => {
                    Some((d.line, target.clone()))
                }
                _ => None,
            })
            .collect();
        broken.sort();
        let expected: Vec<(usize, String)> = [
            (5, "missing-tip"),
            (9, "missing-quote"),
            (13, "missing-quote-unstyled"),
            (16, "missing-md-quote"),
            (18, "missing-dlist-inline"),
            (21, "missing-dlist-next"),
            (23, "missing-dlist-term"),
            (27, "missing-dlist-continuation"),
            (29, "missing-fn"),
            (31, "missing-fn-id"),
            (33, "missing-fn-comma"),
            (35, "missing-control"),
            (38, "missing-dlist-indented"),
            (41, "missing-nested-indent"),
            (45, "missing-dlist-indented-blank"),
        ]
        .into_iter()
        .map(|(l, t)| (l, t.to_string()))
        .collect();
        assert_eq!(broken, expected);
    }

    #[test]
    fn test_update_file_reindexes_dropped_and_added_include_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let index = root.join("index.adoc");
        let child = root.join("child.adoc");
        fs::write(&index, "= Main\n\ninclude::child.adoc[]\n").unwrap();
        fs::write(&child, "= Child Title\n\nText.\n").unwrap();

        let mut ws = Workspace::build(&root, &index);
        assert!(ws.reference_index().lookup("_child_title").is_some());

        // Parent drops the include: the child is a top-level document again.
        let none = HashMap::new();
        let dropped = HashMap::from([(index.clone(), Arc::new("= Main\n".to_string()))]);
        ws.update_file(&index, &dropped);
        assert!(ws.reference_index().lookup("_child_title").is_none());

        // Parent includes it again.
        ws.update_file(&index, &none);
        assert!(ws.reference_index().lookup("_child_title").is_some());
    }

    #[test]
    fn test_ensure_and_remove_file_reindex_include_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let index = root.join("index.adoc");
        let child = root.join("child.adoc");
        fs::write(&index, "= Main\n\ninclude::child.adoc[]\n").unwrap();
        fs::write(&child, "= Child Title\n\nText.\n").unwrap();

        let none = HashMap::new();
        let mut ws = Workspace::new(&root);
        ws.ensure_file(&child, &none);
        assert!(ws.reference_index().lookup("_child_title").is_none());
        ws.ensure_file(&index, &none);
        assert!(ws.reference_index().lookup("_child_title").is_some());

        ws.remove_file(&index);
        assert!(ws.reference_index().lookup("_child_title").is_none());
    }

    /// `[#id]` / `[id=...]` lines register block IDs, and only the last ID
    /// above a block counts. Expected set from the Asciidoctor oracle
    /// (`INFO: possible invalid reference`), committed fixture
    /// `tests/fixtures/block_id_attrs`.
    #[test]
    fn test_fixture_block_ids_from_attribute_lists() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/block_id_attrs")
            .canonicalize()
            .unwrap();
        let index = root.join("index.adoc");

        let ws = Workspace::build(&root, &index);

        let mut broken: Vec<String> = ws
            .diagnostics_for(&index)
            .into_iter()
            .filter_map(|d| match &d.kind {
                crate::types::DiagnosticKind::BrokenXref { target } => Some(target.clone()),
                _ => None,
            })
            .collect();
        broken.sort();
        assert_eq!(broken, ["b", "c", "e", "g", "i", "k", "x"]);
    }

    /// `anchor:id[]` / `anchor:id[reftext]` register inline anchors. Expected
    /// set from the Asciidoctor oracle (`INFO: possible invalid reference`
    /// for nothere, esc, 1digit, `sp ace`; `WARNING: id assigned to anchor
    /// already in use: dup`), committed fixture
    /// `tests/fixtures/inline_anchor_macro`.
    #[test]
    fn test_fixture_anchor_macro_registers_inline_anchor() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/inline_anchor_macro")
            .canonicalize()
            .unwrap();
        let index = root.join("index.adoc");

        let ws = Workspace::build(&root, &index);

        let mut broken: Vec<String> = ws
            .diagnostics_for(&index)
            .into_iter()
            .filter_map(|d| match &d.kind {
                crate::types::DiagnosticKind::BrokenXref { target } => Some(target.clone()),
                _ => None,
            })
            .collect();
        broken.sort();
        assert_eq!(broken, ["1digit", "esc", "nothere", "sp ace"]);
        let duplicates = ws
            .diagnostics_for(&index)
            .into_iter()
            .filter(|d| {
                matches!(
                    &d.kind,
                    crate::types::DiagnosticKind::DuplicateAnchor { .. }
                )
            })
            .count();
        assert_eq!(duplicates, 1);
        assert_eq!(ws.reference_index().lookup("plain").unwrap().line, 5);
        assert_eq!(ws.reference_index().lookup("withtext").unwrap().line, 5);
    }

    /// The adversarial fixture's `[#intro-audience]` above a section
    /// resolves from another chapter (committed fixture
    /// `tests/fixtures/adversarial_project`).
    #[test]
    fn test_fixture_section_id_from_attribute_list_resolves() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/adversarial_project")
            .canonicalize()
            .unwrap();
        let index = root.join("index.adoc");
        let setup = root.join("chapters/setup.adoc");

        let ws = Workspace::build(&root, &index);

        let broken = ws
            .diagnostics_for(&setup)
            .into_iter()
            .filter(|d| {
                matches!(&d.kind, crate::types::DiagnosticKind::BrokenXref { target } if target == "intro-audience")
            })
            .count();
        assert_eq!(broken, 0);
        let loc = ws.reference_index().lookup("intro-audience").unwrap();
        assert!(loc.file.ends_with("chapters/intro.adoc"));
        assert_eq!(loc.line, 9);
    }

    /// Builds a root whose `index.adoc` includes `chapters/extra.adoc` and
    /// references `extra-anchor` before that file exists.
    fn setup_include_target_created_later() -> (tempfile::TempDir, PathBuf, Workspace) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("chapters")).unwrap();
        fs::write(root.join("chapters/intro.adoc"), "== Intro\n").unwrap();
        fs::write(
            root.join("index.adoc"),
            "= I\n\ninclude::chapters/extra.adoc[]\n\nSee <<extra-anchor>>.\n",
        )
        .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();
        let index = root.join("index.adoc");
        let kinds = |ws: &Workspace| -> Vec<crate::types::DiagnosticKind> {
            ws.diagnostics_for(&index)
                .iter()
                .map(|d| d.kind.clone())
                .collect()
        };
        assert!(
            kinds(&ws)
                .iter()
                .any(|k| matches!(k, crate::types::DiagnosticKind::BrokenInclude { .. })),
            "precondition: include is broken"
        );
        assert!(
            kinds(&ws).iter().any(|k| matches!(
                k,
                crate::types::DiagnosticKind::BrokenXref { target } if target == "extra-anchor"
            )),
            "precondition: xref is broken"
        );
        (tmp, root, ws)
    }

    fn assert_extra_target_indexed(ws: &Workspace, root: &Path) {
        let index = root.join("index.adoc");
        assert!(
            !ws.diagnostics_for(&index)
                .iter()
                .any(|d| matches!(d.kind, crate::types::DiagnosticKind::BrokenXref { .. })),
            "xref must resolve: {:?}",
            ws.diagnostics_for(&index)
        );
        assert!(ws.documents.contains_key(&root.join("chapters/extra.adoc")));
        assert!(ws.reference_index.lookup("extra-anchor").is_some());
    }

    /// An include target created on disk after the includer was parsed is
    /// indexed by the next re-parse of the includer (no watcher event needed).
    #[test]
    fn test_update_file_indexes_include_target_created_after_parse() {
        let (_tmp, root, mut ws) = setup_include_target_created_later();
        fs::write(
            root.join("chapters/extra.adoc"),
            "[[extra-anchor]]\n== Extra\n",
        )
        .unwrap();
        ws.update_file(&root.join("index.adoc"), &HashMap::new());
        ws.recalculate_diagnostics();
        assert_extra_target_indexed(&ws, &root);
    }

    /// The includer's stale edge is enough: `recalculate_diagnostics` alone
    /// picks up a target that appeared after the includer was parsed.
    #[test]
    fn test_recalculate_indexes_include_target_created_after_parse() {
        let (_tmp, root, mut ws) = setup_include_target_created_later();
        fs::write(
            root.join("chapters/extra.adoc"),
            "[[extra-anchor]]\n== Extra\n",
        )
        .unwrap();
        ws.recalculate_diagnostics();
        assert_extra_target_indexed(&ws, &root);
    }

    /// A target that includes another new file pulls it in transitively.
    #[test]
    fn test_recalculate_indexes_new_include_targets_transitively() {
        let (_tmp, root, mut ws) = setup_include_target_created_later();
        fs::write(
            root.join("chapters/extra.adoc"),
            "[[extra-anchor]]\n== Extra\n\ninclude::extra2.adoc[]\n",
        )
        .unwrap();
        fs::write(
            root.join("chapters/extra2.adoc"),
            "[[extra2-anchor]]\n== Two\n",
        )
        .unwrap();
        ws.recalculate_diagnostics();
        assert_extra_target_indexed(&ws, &root);
        assert!(ws.reference_index.lookup("extra2-anchor").is_some());
    }

    fn has_broken(ws: &Workspace, file: &Path) -> (bool, bool) {
        let d = ws.diagnostics_for(file);
        (
            d.iter()
                .any(|d| matches!(d.kind, crate::types::DiagnosticKind::BrokenInclude { .. })),
            d.iter()
                .any(|d| matches!(d.kind, crate::types::DiagnosticKind::BrokenXref { .. })),
        )
    }

    fn count_extra(ws: &Workspace) -> usize {
        ws.documents
            .keys()
            .filter(|k| k.file_name().is_some_and(|n| n == "extra.adoc"))
            .count()
    }

    /// Created, indexed, then deleted: both errors return, no orphan stays.
    #[test]
    fn test_recalculate_deleted_walk_indexed_target_breaks_again() {
        let (_tmp, root, mut ws) = setup_include_target_created_later();
        let extra = root.join("chapters/extra.adoc");
        fs::write(&extra, "[[extra-anchor]]\n== Extra\n").unwrap();
        ws.recalculate_diagnostics();
        assert_extra_target_indexed(&ws, &root);
        fs::remove_file(&extra).unwrap();
        ws.remove_file(&extra);
        ws.recalculate_diagnostics();
        assert_eq!(has_broken(&ws, &root.join("index.adoc")), (true, true));
        assert_eq!(count_extra(&ws), 0);
    }

    /// Only `.adoc` targets are parsed; a created YAML include is left alone.
    #[test]
    fn test_recalculate_does_not_index_non_adoc_include_target() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("snippets")).unwrap();
        fs::write(
            root.join("index.adoc"),
            "= I\n\ninclude::snippets/config.yml[]\n",
        )
        .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        fs::write(root.join("snippets/config.yml"), "[[yml-anchor]]\n").unwrap();
        ws.recalculate_diagnostics();
        assert_eq!(ws.documents.len(), 1);
    }

    /// An open buffer keeps its unsaved content; no disk copy shadows it.
    #[test]
    fn test_recalculate_keeps_open_buffer_include_target() {
        let (_tmp, root, mut ws) = setup_include_target_created_later();
        let extra = root.join("chapters/extra.adoc");
        let overrides: HashMap<PathBuf, Arc<String>> = HashMap::from([(
            extra.clone(),
            Arc::new("[[buffer-anchor]]\n== B\n".to_string()),
        )]);
        fs::write(&extra, "[[disk-anchor]]\n== D\n").unwrap();
        ws.ensure_file(&extra, &overrides);
        ws.recalculate_diagnostics();
        assert!(ws.reference_index.lookup("buffer-anchor").is_some());
        assert!(ws.reference_index.lookup("disk-anchor").is_none());
        assert_eq!(count_extra(&ws), 1);
    }

    /// Include targets outside the root are indexed too (Asciidoctor reads them).
    #[test]
    fn test_recalculate_indexes_include_target_outside_root() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let root = base.join("proj");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("index.adoc"),
            "= I\n\ninclude::../shared/extra.adoc[]\n\nSee <<extra-anchor>>.\n",
        )
        .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        fs::create_dir_all(base.join("shared")).unwrap();
        fs::write(base.join("shared/extra.adoc"), "[[extra-anchor]]\n== E\n").unwrap();
        ws.recalculate_diagnostics();
        assert!(ws.reference_index.lookup("extra-anchor").is_some());
        assert_eq!(has_broken(&ws, &root.join("index.adoc")), (false, false));
    }

    /// The include path crosses a directory symlink: one document, keyed
    /// canonically, and deleting it breaks both again.
    #[cfg(unix)]
    #[test]
    fn test_recalculate_include_target_via_symlinked_dir_has_one_key() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("chapters")).unwrap();
        fs::write(
            root.join("index.adoc"),
            "= I\n\ninclude::chapters/extra.adoc[]\n\nSee <<extra-anchor>>.\n",
        )
        .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();
        let extra = root.join("real/extra.adoc");
        fs::write(&extra, "[[extra-anchor]]\n== Extra\n").unwrap();
        ws.recalculate_diagnostics();
        assert_eq!(count_extra(&ws), 1);
        assert!(ws.documents.contains_key(&extra));
        assert_eq!(has_broken(&ws, &root.join("index.adoc")), (false, false));
        // Re-parse of the includer must not add a second key.
        ws.update_file(&root.join("index.adoc"), &HashMap::new());
        ws.recalculate_diagnostics();
        assert_eq!(count_extra(&ws), 1);
        fs::remove_file(&extra).unwrap();
        ws.remove_file(&extra);
        ws.recalculate_diagnostics();
        assert_eq!(has_broken(&ws, &root.join("index.adoc")), (true, true));
        assert_eq!(count_extra(&ws), 0);
    }

    /// Project opened through a symlinked root, file created, CREATED event
    /// path (`ensure_file` on the canonical path, then recalculate).
    #[cfg(unix)]
    #[test]
    fn test_created_event_with_symlinked_root_has_one_key() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let real = base.join("realroot");
        fs::create_dir_all(real.join("chapters")).unwrap();
        std::os::unix::fs::symlink(&real, base.join("linkroot")).unwrap();
        fs::write(
            real.join("index.adoc"),
            "= I\n\ninclude::chapters/extra.adoc[]\n\nSee <<extra-anchor>>.\n",
        )
        .unwrap();
        let link_index = base.join("linkroot/index.adoc");
        let mut ws = Workspace::new(&base.join("linkroot"));
        // Like did_open: addressed by the link, keyed canonically.
        ws.set_doc_dir(&real.join("index.adoc"), &base.join("linkroot"));
        ws.ensure_file(&real.join("index.adoc"), &HashMap::new());
        ws.recalculate_diagnostics();
        let index = real.join("index.adoc");
        assert!(link_index.exists());
        assert_eq!(has_broken(&ws, &index), (true, true));
        let extra = real.join("chapters/extra.adoc");
        fs::write(&extra, "[[extra-anchor]]\n== Extra\n").unwrap();
        ws.ensure_file(&extra, &HashMap::new());
        ws.recalculate_diagnostics();
        assert_eq!(count_extra(&ws), 1);
        assert_eq!(has_broken(&ws, &index), (false, false));
        fs::remove_file(&extra).unwrap();
        ws.remove_file(&extra);
        ws.recalculate_diagnostics();
        assert_eq!(has_broken(&ws, &index), (true, true));
        assert_eq!(count_extra(&ws), 0);
    }

    /// Duplicate-anchor and circular-include messages, sorted.
    fn path_bearing_messages(root: &Path) -> Vec<String> {
        let mut ws = Workspace::new(root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();
        let mut messages: Vec<String> = ws
            .all_diagnostics()
            .into_iter()
            .map(|d| d.kind.to_string())
            .filter(|m| m.starts_with("Duplicate anchor") || m.starts_with("Circular include"))
            .collect();
        messages.sort();
        messages
    }

    /// Shared assertions: two root-relative messages, none containing a
    /// `forbidden` absolute prefix.
    fn assert_root_relative(messages: &[String], forbidden: &[&str]) {
        assert_eq!(messages.len(), 2, "got: {messages:?}");
        for m in messages {
            for f in forbidden {
                assert!(!m.contains(f), "absolute path in: {m}");
            }
        }
        assert!(
            messages
                .iter()
                .any(|m| m.contains("first defined in chapters/")),
            "got: {messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|m| m.contains("cycle through partials/")),
            "got: {messages:?}"
        );
    }

    /// Duplicate-anchor and circular-include messages name the other file
    /// relative to the workspace root, never absolute (committed fixture
    /// `tests/fixtures/diagnostic_paths`).
    #[test]
    fn test_fixture_diagnostic_messages_use_workspace_relative_paths() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/diagnostic_paths")
            .canonicalize()
            .unwrap();
        let messages = path_bearing_messages(&root);

        assert_root_relative(&messages, &[&root.display().to_string()]);
    }

    /// A root reached through a symlink or a `..` segment is not canonical,
    /// while document paths are; messages must still be root-relative.
    #[cfg(unix)]
    #[test]
    fn test_non_canonical_root_still_yields_relative_paths() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/diagnostic_paths")
            .canonicalize()
            .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&fixture, &link).unwrap();
        let dotdot = fixture.join("../diagnostic_paths");

        for root in [link, dotdot] {
            let messages = path_bearing_messages(&root);
            assert_root_relative(
                &messages,
                &[&fixture.display().to_string(), &root.display().to_string()],
            );
        }
    }

    /// A file outside the workspace root (reached via
    /// `include::../shared/x.adoc[]`) is shown root-relative (`../shared/x.adoc`),
    /// not with its absolute path (fixture `tests/fixtures/diagnostic_paths_outside`).
    #[test]
    fn test_outside_root_path_is_root_relative() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/diagnostic_paths_outside")
            .canonicalize()
            .unwrap();
        let messages = path_bearing_messages(&base.join("root"));
        let base_str = base.display().to_string();
        assert!(
            messages.iter().any(|m| m.contains("../shared/x.adoc")),
            "expected ../shared/x.adoc, got: {messages:?}"
        );
        for m in &messages {
            assert!(!m.contains(&base_str), "absolute path in: {m}");
        }
    }
}
