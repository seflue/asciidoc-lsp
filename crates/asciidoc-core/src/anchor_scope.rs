//! The IDs one top-level document defines. Asciidoctor
//! knows an ID only within the document it converts: the top-level document
//! and its includes. The first definition wins, a section whose generated ID
//! is taken gets a numeric suffix, and only an explicit ID on a taken name is
//! a duplicate.

use std::cell::{OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::Ordering;

use crate::paths::{canonicalize_if_exists, join_doc_path, resolve_doc_target};
use crate::reading::{read_in_order, ReadingVisitor};
use crate::references::{effective_id_separator, generate_section_id, parse_xref_target};
use crate::types::{
    Anchor, AnchorKind, AnchorLocation, DiagnosticKind, DocumentModel, FileLocation, IncludeEdge,
    ResolvedXref, Section, Xref,
};
use crate::workspace::{is_own_top_level, top_level_documents_in, AttributeScope, Workspace};

/// An explicit ID that a taken name rejects.
#[derive(Debug, Clone)]
pub struct DuplicateId {
    pub id: String,
    pub first: AnchorLocation,
    pub second: AnchorLocation,
}

/// The IDs of one top-level document. Each ID maps to its first definition in
/// reading order; the map itself is unordered.
#[derive(Debug, Clone)]
pub struct AnchorScope {
    document: PathBuf,
    /// The files that count as part of the document for `file#id` xrefs.
    files: HashSet<PathBuf>,
    /// The files the document reads in full: the top-level document and the
    /// files a chain of whole includes (`IncludeSelection::Whole`) reaches.
    whole: HashSet<PathBuf>,
    ids: HashMap<String, AnchorLocation>,
    /// The text a natural xref (`<<Section title>>`) matches, mapped to its
    /// ID: the reftext of an anchor, or the title of a section without one.
    /// The first definition of a text wins.
    xreftexts: HashMap<String, String>,
    duplicates: Vec<DuplicateId>,
}

impl AnchorScope {
    /// The top-level document the scope belongs to.
    #[must_use]
    pub fn document(&self) -> &Path {
        &self.document
    }

    /// Where `id` is first defined in the document.
    #[must_use]
    pub fn lookup(&self, id: &str) -> Option<&AnchorLocation> {
        self.ids.get(id)
    }

    /// The ID that a natural xref names, with its first definition.
    /// Asciidoctor matches a target that is no ID, has no `#` and contains a
    /// space or an uppercase letter against the reftexts and section titles.
    fn lookup_natural(&self, target: &str) -> Option<(&str, &AnchorLocation)> {
        let natural = !target.contains('#')
            && (target.contains(' ') || target.chars().any(char::is_uppercase));
        if !natural {
            return None;
        }
        let id = self.xreftexts.get(target)?;
        Some((id, self.ids.get(id)?))
    }

    /// Every ID of the document with its first definition.
    pub fn ids(&self) -> impl Iterator<Item = (&str, &AnchorLocation)> {
        self.ids
            .iter()
            .map(|(id, location)| (id.as_str(), location))
    }

    /// The explicit IDs defined again after their first definition.
    #[must_use]
    pub fn duplicates(&self) -> &[DuplicateId] {
        &self.duplicates
    }

    /// `file` is the top-level document or a file that a full include (no
    /// `tag`, `tags`, `lines`) reaches, directly or below a region include.
    /// A file read only in part is not part of the document for xrefs, though
    /// the walk reads its IDs.
    #[must_use]
    pub fn contains_file(&self, file: &Path) -> bool {
        self.files.contains(file)
    }

    /// The document reads every line of `file`: it is the top-level document
    /// or lies below whole includes only. For any other file the walk knows
    /// no tag regions, so what the document reads of it is unknown, and a
    /// missing ID there is no proof of a broken xref.
    #[must_use]
    pub fn reads_whole(&self, file: &Path) -> bool {
        self.whole.contains(file)
    }
}

/// The ID catalogs of one request or pass over the workspace, each read once,
/// with the top-level documents of every file computed once. The cache
/// borrows the workspace, so no mutator (they all take `&mut`) can run while
/// it lives and nothing needs invalidating.
pub struct CatalogCache<'ws> {
    workspace: &'ws Workspace,
    catalogs: RefCell<HashMap<PathBuf, Rc<AnchorScope>>>,
    roots: OnceCell<HashMap<PathBuf, Vec<PathBuf>>>,
}

impl<'ws> CatalogCache<'ws> {
    /// An empty cache; a catalog is read on first use.
    #[must_use]
    pub fn new(workspace: &'ws Workspace) -> Self {
        Self {
            workspace,
            catalogs: RefCell::new(HashMap::new()),
            roots: OnceCell::new(),
        }
    }

    /// `IncludeGraph::doc_roots` of the workspace.
    pub(crate) fn roots(&self) -> &HashMap<PathBuf, Vec<PathBuf>> {
        self.roots.get_or_init(|| {
            self.workspace
                .include_graph
                .doc_roots(&self.workspace.documents)
        })
    }

    /// The catalog of `document`, read as a top-level document.
    pub fn get(&self, document: &Path) -> Rc<AnchorScope> {
        let document = self.workspace.indexed_path(document);
        if let Some(scope) = self.catalogs.borrow().get(&document) {
            return Rc::clone(scope);
        }
        let scope = Rc::new(self.workspace.read_anchor_scope(&document));
        self.catalogs
            .borrow_mut()
            .insert(document, Rc::clone(&scope));
        scope
    }

    /// The catalog of each top-level document that reaches `file`, in the
    /// order of [`Workspace::scopes_at`]. A file no document includes is its
    /// own top-level document.
    pub fn scopes_at(&self, file: &Path) -> Vec<Rc<AnchorScope>> {
        let file = self.workspace.indexed_path(file);
        top_level_documents_in(self.roots(), &file)
            .unwrap_or_else(|| vec![file])
            .iter()
            .map(|document| self.get(document))
            .collect()
    }

    /// Whether other documents include `file`, so it has readers besides
    /// itself.
    pub fn is_included(&self, file: &Path) -> bool {
        let file = self.workspace.indexed_path(file);
        top_level_documents_in(self.roots(), &file).is_some()
    }

    /// Where `xref` points when read in `scope`, one top-level document.
    /// `resolve_file` turns the file part of the target into a path.
    ///
    /// A bare or `#id` target is an ID of the document. `file#id` is internal
    /// when `file` is part of the document, so the ID is searched in the
    /// whole document and may lie in another file (Asciidoctor renders it as
    /// `#id`); otherwise it is a link to the document rendered from `file`,
    /// which Asciidoctor does not check and this does, against the catalog
    /// of `file` read as a document of its own.
    ///
    /// # Errors
    /// Returns a `DiagnosticKind::BrokenXref` if the file or the ID cannot be
    /// found.
    pub fn resolve_xref_in(
        &self,
        scope: &AnchorScope,
        xref: &Xref,
        resolve_file: impl FnOnce(&str) -> Option<PathBuf>,
    ) -> Result<ResolvedXref, DiagnosticKind> {
        let broken = || DiagnosticKind::BrokenXref {
            target: xref.target.clone(),
            documents: Vec::new(),
        };
        let (file_part, anchor_part) = parse_xref_target(&xref.target);
        let located = |id: &str, location: &AnchorLocation| ResolvedXref {
            file: location.file.clone(),
            line: location.line,
            kind: Some(location.kind),
            id: Some(id.to_string()),
        };
        let Some(file_ref) = file_part else {
            return anchor_part
                .and_then(|id| {
                    scope
                        .lookup(id)
                        .map(|location| (id, location))
                        .or_else(|| scope.lookup_natural(&xref.target))
                })
                .map(|(id, location)| located(id, location))
                .ok_or_else(broken);
        };
        // Only the path Asciidoctor reads names the file (ADR-009): a file
        // of the same name in another folder is no target.
        let resolved = resolve_file(file_ref).ok_or_else(broken)?;
        let (file, _) = self
            .workspace
            .documents
            .get_key_value(&resolved)
            .ok_or_else(broken)?;
        let Some(id) = anchor_part else {
            return Ok(ResolvedXref {
                file: file.clone(),
                line: 1,
                kind: None,
                id: None,
            });
        };
        let location = if scope.contains_file(file) {
            scope.lookup(id).map(|location| located(id, location))
        } else {
            self.get(file)
                .lookup(id)
                .map(|location| located(id, location))
        };
        location.ok_or_else(broken)
    }

    /// [`Self::resolve_xref_in`] for an xref in a file `scope` reads, with the
    /// file part resolved as Asciidoctor resolves it when it converts the
    /// document `scope`: from that document's folder, in its attribute scope
    /// among `attribute_scopes` (the scopes of the xref's line, needed only
    /// when the target has an attribute reference).
    ///
    /// # Errors
    /// Returns a `DiagnosticKind::BrokenXref` if the file or the ID cannot be
    /// found.
    pub(crate) fn resolve_read(
        &self,
        scope: &AnchorScope,
        xref: &Xref,
        attribute_scopes: &[AttributeScope<'_>],
    ) -> Result<ResolvedXref, DiagnosticKind> {
        let attributes = attribute_scopes
            .iter()
            .find(|candidate| candidate.document() == scope.document());
        self.resolve_xref_in(scope, xref, |file| {
            self.workspace
                .resolve_xref_file(scope.document(), attributes, file)
        })
    }

    /// The xrefs of `id` that point at one of the `definitions`: those that
    /// resolve to it in at least one top-level document that reads them, so
    /// an xref to the same ID in another document is no reference. Each
    /// location carries the column of its xref, so two xrefs on one line are
    /// told apart.
    #[must_use]
    pub fn anchor_references(&self, id: &str, definitions: &[AnchorLocation]) -> Vec<FileLocation> {
        let workspace = self.workspace;
        let mut references = Vec::new();
        for (path, doc) in &workspace.documents {
            let named = doc
                .xrefs
                .iter()
                .filter(|xref| parse_xref_target(&xref.target).1 == Some(id));
            for xref in named {
                let attribute_scopes = if xref.target.contains('{') {
                    workspace.scopes_at(path, xref.line)
                } else {
                    Vec::new()
                };
                let reaches_definition = self.scopes_at(path).iter().any(|scope| {
                    self.resolve_read(scope, xref, &attribute_scopes)
                        .is_ok_and(|resolved| {
                            definitions.iter().any(|definition| {
                                resolved.file == definition.file && resolved.line == definition.line
                            })
                        })
                });
                if reaches_definition {
                    let (col, end_col) = xref.id_span().map_or((0, 0), |(_, c, e)| (c, e));
                    references.push(FileLocation::new(path.clone(), xref.line, col, end_col));
                }
            }
        }
        references
    }

    /// The catalog of every top-level document of the workspace, in path
    /// order.
    pub(crate) fn all(&self) -> Vec<Rc<AnchorScope>> {
        let roots = self.roots();
        let mut documents: Vec<&PathBuf> = self
            .workspace
            .documents
            .keys()
            .filter(|file| is_own_top_level(roots.get(*file).map(Vec::as_slice), file))
            .collect();
        documents.sort();
        documents
            .into_iter()
            .map(|document| self.get(document))
            .collect()
    }
}

impl Workspace {
    /// The ID catalog of each top-level document that reaches `file`, in the
    /// order of [`Self::scopes_at`]. A file no document includes is its own
    /// top-level document.
    #[must_use]
    pub fn anchor_scopes_at(&self, file: &Path) -> Vec<AnchorScope> {
        let file = self.indexed_path(file);
        self.top_level_documents_or_self(&file)
            .iter()
            .map(|document| self.read_anchor_scope(document))
            .collect()
    }

    /// The ID catalog of the document `document`, read as a top-level
    /// document.
    #[must_use]
    pub fn anchor_scope_of(&self, document: &Path) -> AnchorScope {
        self.read_anchor_scope(&self.indexed_path(document))
    }

    /// The title of the section that `id` names, defined at `location`.
    /// `None` for an anchor outside a section title (paragraph, inline,
    /// block) and for an ID Asciidoctor drops from a run of ID lines, which
    /// names no section. The title is raw, attribute references unresolved.
    #[must_use]
    pub fn anchor_title(&self, id: &str, location: &AnchorLocation) -> Option<&str> {
        let document = self.documents.get(&location.file)?;
        let mut sections = document.sections.iter();
        let section = match location.kind {
            AnchorKind::ImplicitSection => sections.find(|s| s.line == location.line),
            AnchorKind::Explicit | AnchorKind::BlockId => {
                sections.find(|s| s.line >= location.line && s.id.as_deref() == Some(id))
            }
        }?;
        Some(section.title.as_str())
    }

    /// Every definition of an ID in the workspace, once: the first definition
    /// of each ID in each top-level document and the explicit ones after it.
    /// Unlike [`AnchorScope::lookup`] it keeps an ID that several documents
    /// define once per place.
    #[must_use]
    pub fn anchor_definitions(&self) -> Vec<(String, AnchorLocation)> {
        let scopes = CatalogCache::new(self).all();
        let mut seen: HashSet<(&str, &Path, usize)> = HashSet::new();
        let mut definitions = Vec::new();
        for scope in &scopes {
            let repeated = scope
                .duplicates()
                .iter()
                .map(|duplicate| (duplicate.id.as_str(), &duplicate.second));
            for (id, location) in scope.ids().chain(repeated) {
                if seen.insert((id, &location.file, location.line)) {
                    definitions.push((id.to_string(), location.clone()));
                }
            }
        }
        definitions
    }

    /// The file part of an xref read in the top-level document `document`,
    /// as a path. Asciidoctor resolves it from the folder of `document`, also
    /// when the xref stands in an included file. `attributes` is the scope of
    /// `document`, which fills in attribute references in the target.
    pub(crate) fn resolve_xref_file(
        &self,
        document: &Path,
        attributes: Option<&AttributeScope<'_>>,
        file: &str,
    ) -> Option<PathBuf> {
        match (attributes, self.attributes.doc_dir(document)) {
            (Some(attributes), Some(dir)) => Some(canonicalize_if_exists(join_doc_path(
                dir,
                &attributes.substitute(file),
            ))),
            _ => resolve_doc_target(document, file, &self.attributes),
        }
    }

    /// `document` is already an indexed path.
    fn read_anchor_scope(&self, document: &Path) -> AnchorScope {
        self.catalog_builds.fetch_add(1, Ordering::Relaxed);
        let document = document.to_path_buf();
        let mut walk = AnchorWalk {
            workspace: self,
            region_depth: 0,
            scope: AnchorScope {
                document: document.clone(),
                files: HashSet::from([document.clone()]),
                whole: HashSet::from([document.clone()]),
                ids: HashMap::new(),
                xreftexts: HashMap::new(),
                duplicates: Vec::new(),
            },
            region_defined: Definitions::default(),
            plain_read: Definitions::default(),
            reftext_ids: HashSet::new(),
        };
        read_in_order(self, &document, &mut walk);
        walk.scope
    }
}

/// How much of its file an include reads, by Asciidoctor's rule (derived
/// from probes of Asciidoctor 2.0.26; the first of the keys `lines`, `tag`,
/// `tags` that is present decides alone, even with an empty value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IncludeSelection {
    /// Every line, or every tag (`**`): the file is part of the document and
    /// a second reading repeats every ID.
    Whole,
    /// Everything but some named tags (`**;!t`, `!t`): the file is part of
    /// the document for xrefs, but not every line is read.
    AllButTags,
    /// A part of the file (`lines`, a named tag, `*`, `!**`): Asciidoctor
    /// renders `file#id` as a link to the file's own document.
    Part,
}

pub(crate) fn include_selection(edge: &IncludeEdge) -> IncludeSelection {
    let value = |key: &str| {
        edge.attributes
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.trim().trim_matches('"'))
    };
    if let Some(lines) = value("lines") {
        return if lines.is_empty() {
            IncludeSelection::Whole
        } else {
            IncludeSelection::Part
        };
    }
    let Some(tags) = value("tag").or_else(|| value("tags")) else {
        return IncludeSelection::Whole;
    };
    let entries: Vec<&str> = tags
        .split([';', ','])
        .map(str::trim)
        .filter(|entry| !entry.is_empty() && *entry != "!")
        .collect();
    let has = |wanted: &str| entries.contains(&wanted);
    let names_a_tag = entries
        .iter()
        .any(|entry| !entry.starts_with('!') && *entry != "*" && *entry != "**");
    if names_a_tag || has("!**") || has("!*") || (has("*") && !has("**")) {
        IncludeSelection::Part
    } else if entries.iter().any(|entry| entry.starts_with('!')) {
        IncludeSelection::AllButTags
    } else {
        IncludeSelection::Whole
    }
}

/// The definitions of files by line, looked up by `&Path` and `&str` without
/// allocating. A definition is an anchor (`Some(id)`) or a section (`None`):
/// one line may define several, and each is read on its own.
#[derive(Default)]
struct Definitions(HashMap<PathBuf, HashMap<usize, Vec<Option<String>>>>);

impl Definitions {
    fn contains(&self, file: &Path, line: usize, id: Option<&str>) -> bool {
        self.0
            .get(file)
            .and_then(|lines| lines.get(&line))
            .is_some_and(|known| known.iter().any(|known| known.as_deref() == id))
    }

    /// Whether the definition was new.
    fn insert(&mut self, file: &Path, line: usize, id: Option<&str>) -> bool {
        if self.contains(file, line, id) {
            return false;
        }
        self.0
            .entry(file.to_path_buf())
            .or_default()
            .entry(line)
            .or_default()
            .push(id.map(str::to_string));
        true
    }
}

/// The ID a definition is keyed by in [`Definitions`]: the ID of an anchor,
/// none for a section.
fn definition_id<'b>(location: &AnchorLocation, id: &'b str) -> Option<&'b str> {
    (location.kind != AnchorKind::ImplicitSection).then_some(id)
}

/// A line of a file that defines an ID.
enum Definition<'a> {
    Anchor(&'a Anchor),
    Section(&'a Section),
}

struct AnchorWalk<'a> {
    workspace: &'a Workspace,
    /// Includes with a region between the top-level document and the file
    /// being read. The walk knows no tag regions, so it cannot tell which
    /// IDs of such a file Asciidoctor reads: it counts a definition it meets
    /// again once, and reports none.
    region_depth: usize,
    /// Definitions first made while `region_depth > 0`. Such a definition may
    /// lie outside the region Asciidoctor reads, so a later reading of it is
    /// no duplicate whichever include comes first.
    region_defined: Definitions,
    /// Definitions met while `region_depth == 0`. Such a reading covers the
    /// whole file, so meeting the definition again is a real second reading
    /// even when a region include read it before. Two definitions on one
    /// line are two readings.
    plain_read: Definitions,
    /// The IDs that carry a reftext, even one an earlier ID already holds: a
    /// section among them is not matched by its title.
    reftext_ids: HashSet<String>,
    scope: AnchorScope,
}

impl<'a> ReadingVisitor<'a> for AnchorWalk<'a> {
    type Item = Definition<'a>;

    fn items(&self, _file: &Path, doc: &'a DocumentModel) -> Vec<(usize, Definition<'a>)> {
        doc.anchors
            .iter()
            .map(|anchor| (anchor.line, Definition::Anchor(anchor)))
            .chain(
                doc.sections
                    .iter()
                    .map(|section| (section.line, Definition::Section(section))),
            )
            .collect()
    }

    fn item(&mut self, stack: &[PathBuf], file: &Path, item: Definition<'a>) {
        match item {
            Definition::Anchor(anchor) => {
                let plain_reread = self.note_plain_reading(file, anchor.line, Some(&anchor.id));
                self.define_explicit(file, anchor, plain_reread);
                if let Some(reftext) = &anchor.reftext {
                    self.reftext_ids.insert(anchor.id.clone());
                    self.define_xreftext(reftext, &anchor.id);
                }
            }
            Definition::Section(section) => {
                // The header title of the top-level document gets no ID. An
                // included level-0 title that Asciidoctor would take as the
                // document title (a top-level file without a header of its
                // own) still gets one here: a false negative, accepted.
                let is_document_header = section.is_header && stack.len() == 1;
                if let Some(id) = &section.id {
                    self.define_section_title(&section.title, id);
                } else if !is_document_header {
                    let plain_reread = self.note_plain_reading(file, section.line, None);
                    self.define_section(file, section, plain_reread);
                }
            }
        }
    }

    fn enter(&mut self, edge: &IncludeEdge) {
        // A file read only in part (`tag`, `tags`, `lines`, see
        // `IncludeSelection::Part`) is no part of the document for `file#id`
        // xrefs: Asciidoctor renders them as links to the file's own
        // document. An include below a region include counts as part: the
        // walk cannot tell whether it lies inside the region, and a false
        // "external" would give false broken xrefs.
        let selection = include_selection(edge);
        if selection != IncludeSelection::Part
            && self.workspace.documents.contains_key(&edge.target)
        {
            self.scope.files.insert(edge.target.clone());
        }
        if selection == IncludeSelection::Whole {
            if self.region_depth == 0 && self.workspace.documents.contains_key(&edge.target) {
                self.scope.whole.insert(edge.target.clone());
            }
        } else {
            self.region_depth += 1;
        }
    }

    fn leave(&mut self, edge: &IncludeEdge) {
        if include_selection(edge) != IncludeSelection::Whole {
            self.region_depth -= 1;
        }
    }
}

impl AnchorWalk<'_> {
    /// Records a reading of the definition `id` (`None`: a section) on `line`
    /// outside any region; true when an earlier plain reading met it too.
    fn note_plain_reading(&mut self, file: &Path, line: usize, id: Option<&str>) -> bool {
        self.region_depth == 0 && !self.plain_read.insert(file, line, id)
    }

    /// The current reading may have covered only part of its file, or the
    /// one that defined `first` as `id` did.
    fn may_miss_region(&self, first: &AnchorLocation, id: &str) -> bool {
        self.region_depth > 0
            || self
                .region_defined
                .contains(&first.file, first.line, definition_id(first, id))
    }

    /// A reading of `location` that meets an earlier one is ignorable when
    /// either reading may have covered only part of the file and both are
    /// the same definition. `id` is the ID `taken` holds.
    fn is_region_reread(
        &self,
        taken: &AnchorLocation,
        location: &AnchorLocation,
        id: &str,
    ) -> bool {
        is_same_definition(taken, location) && self.may_miss_region(taken, id)
    }

    fn register(&mut self, id: String, location: AnchorLocation) {
        if self.region_depth > 0 {
            self.region_defined.insert(
                &location.file,
                location.line,
                definition_id(&location, &id),
            );
        }
        self.scope.ids.insert(id, location);
    }

    fn define_xreftext(&mut self, text: &str, id: &str) {
        self.scope
            .xreftexts
            .entry(text.to_string())
            .or_insert_with(|| id.to_string());
    }

    /// A section with a reftext is matched by the reftext only.
    fn define_section_title(&mut self, title: &str, id: &str) {
        if !self.reftext_ids.contains(id) {
            self.define_xreftext(title, id);
        }
    }

    /// No duplicate when either reading may have covered only part of its
    /// file, whatever the line: the walk knows no tag regions, and the ID may
    /// lie outside the tag Asciidoctor reads. A real collision inside a tag
    /// is missed (false negative), preferred over a false warning. The first
    /// definition stays in `ids`, so `lookup` may point at a region-read
    /// file. Two plain readings of one line are always a duplicate.
    fn define_explicit(&mut self, file: &Path, anchor: &Anchor, plain_reread: bool) {
        let location = AnchorLocation {
            file: file.to_path_buf(),
            line: anchor.line,
            kind: AnchorKind::Explicit,
        };
        let Some(first) = self.scope.ids.get(&anchor.id) else {
            self.register(anchor.id.clone(), location);
            return;
        };
        if !plain_reread && self.may_miss_region(first, &anchor.id) {
            return;
        }
        let first = first.clone();
        self.scope.duplicates.push(DuplicateId {
            id: anchor.id.clone(),
            first,
            second: location,
        });
    }

    fn define_section(&mut self, file: &Path, section: &Section, plain_reread: bool) {
        let registry = &self.workspace.attributes;
        let prefix = registry.resolve(file, "idprefix").unwrap_or("_");
        let separator =
            effective_id_separator(registry.resolve(file, "idseparator").unwrap_or("_"));
        let base = generate_section_id(&section.title, prefix, separator);
        let location = AnchorLocation {
            file: file.to_path_buf(),
            line: section.line,
            kind: AnchorKind::ImplicitSection,
        };
        let mut id = base.clone();
        let mut suffix = 2;
        while let Some(taken) = self.scope.ids.get(&id) {
            // A section re-read under a region is dropped, not suffixed:
            // Asciidoctor may resolve `<<x_2>>` there, the catalog calls it
            // unknown (accepted trade-off).
            if !plain_reread && self.is_region_reread(taken, &location, &id) {
                return;
            }
            id = format!("{base}{separator}{suffix}");
            suffix += 1;
        }
        self.define_section_title(&section.title, &id);
        self.register(id, location);
    }
}

/// A section is never the same definition as an anchor on its line.
fn is_same_definition(a: &AnchorLocation, b: &AnchorLocation) -> bool {
    a.file == b.file && a.line == b.line && a.kind == b.kind
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Placement;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/anchor_catalog")
            .canonicalize()
            .unwrap()
    }

    fn catalog(entry: &str) -> AnchorScope {
        let root = fixture();
        let document = root.join(entry);
        let ws = Workspace::build(&root, &document);
        ws.anchor_scope_of(&document)
    }

    fn file_name(location: &AnchorLocation) -> &str {
        location.file.file_name().unwrap().to_str().unwrap()
    }

    /// Asciidoctor: the warning stands at `dup_a.adoc`, the second file read
    /// (`dup_order.adoc` includes `dup_z.adoc` first, the reverse of path
    /// order).
    #[test]
    fn test_catalog_first_definition_in_reading_order_wins() {
        let scope = catalog("dup_order.adoc");

        let first = scope.lookup("shared").unwrap();
        assert_eq!(file_name(first), "dup_z.adoc");
        assert_eq!(scope.duplicates().len(), 1);
        let duplicate = &scope.duplicates()[0];
        assert_eq!(duplicate.id, "shared");
        assert_eq!(file_name(&duplicate.first), "dup_z.adoc");
        assert_eq!(file_name(&duplicate.second), "dup_a.adoc");
    }

    #[test]
    fn test_anchor_scopes_at_included_file_is_the_catalog_of_its_top_level_document() {
        let root = fixture();
        let entry = root.join("dup_order.adoc");
        let ws = Workspace::build(&root, &entry);

        let scopes = ws.anchor_scopes_at(&root.join("dup_a.adoc"));

        assert_eq!(scopes.len(), 1);
        assert_eq!(scopes[0].document(), entry);
        assert!(scopes[0].contains_file(&root.join("dup_z.adoc")));
        let orphan = ws.anchor_scopes_at(&entry);
        assert_eq!(orphan[0].document(), entry);
    }

    /// Asciidoctor: `_foo`, `_foo_2`, `_foo_3`; a taken generated ID never
    /// warns, across files too.
    #[test]
    fn test_catalog_generated_ids_get_numeric_suffix_not_duplicate() {
        let scope = catalog("auto_twice.adoc");

        assert!(scope.duplicates().is_empty());
        assert_eq!(file_name(scope.lookup("_foo").unwrap()), "auto_twice.adoc");
        assert_eq!(scope.lookup("_foo_2").unwrap().line, 5);
        assert_eq!(file_name(scope.lookup("_foo_3").unwrap()), "auto_foo.adoc");
        assert!(scope.lookup("_foo_4").is_none());
    }

    /// Asciidoctor: `[[_foo]]` first, the generated `_foo` of the later
    /// section becomes `_foo_2`, no warning.
    #[test]
    fn test_catalog_explicit_then_generated_moves_generated_aside() {
        let scope = catalog("explicit_then_implicit.adoc");

        // `== Bar` has an explicit ID, so it gets no generated `_bar`.
        assert!(scope.lookup("_bar").is_none());

        assert!(scope.duplicates().is_empty());
        assert!(matches!(
            scope.lookup("_foo").unwrap().kind,
            AnchorKind::Explicit
        ));
        assert!(matches!(
            scope.lookup("_foo_2").unwrap().kind,
            AnchorKind::ImplicitSection
        ));
    }

    /// Asciidoctor: `== Foo` takes `_foo`, the later `[[_foo]]` warns.
    #[test]
    fn test_catalog_generated_then_explicit_is_duplicate_at_explicit() {
        let scope = catalog("implicit_then_explicit.adoc");

        assert_eq!(scope.duplicates().len(), 1);
        let duplicate = &scope.duplicates()[0];
        assert_eq!(duplicate.id, "_foo");
        assert!(matches!(duplicate.first.kind, AnchorKind::ImplicitSection));
        assert_eq!(duplicate.second.line, 5);
        assert!(matches!(
            scope.lookup("_foo").unwrap().kind,
            AnchorKind::ImplicitSection
        ));
    }

    /// Asciidoctor appends `:idseparator:` to the suffix: `_foo-bar-2`.
    #[test]
    fn test_catalog_suffix_uses_idseparator() {
        let scope = catalog("idseparator.adoc");

        assert!(scope.lookup("_foo-bar").is_some());
        assert!(scope.lookup("_foo-bar-2").is_some());
        assert!(scope.lookup("_foo_bar_2").is_none());
    }

    /// Asciidoctor: an empty `:idseparator:` drops the spaces, the duplicate
    /// suffix follows without separator (`_foobar2`). Must terminate.
    #[test]
    fn test_catalog_empty_idseparator_terminates_and_joins_suffix() {
        let scope = catalog("idseparator_empty.adoc");

        assert!(scope.lookup("_foobar").is_some());
        assert!(scope.lookup("_foobar2").is_some());
    }

    /// Asciidoctor uses only the first character of a multi-char separator,
    /// in the ID and in the duplicate suffix.
    #[test]
    fn test_catalog_multi_char_idseparator_uses_first_char_only() {
        let scope = catalog("idseparator_multi.adoc");

        assert!(scope.lookup("_foo-bar").is_some());
        assert!(scope.lookup("_foo-bar-2").is_some());
        assert!(scope.lookup("_foo--bar").is_none());
    }

    /// Multi-char separator `éa`: only `é` is used, sliced at a multi-byte
    /// boundary, in the ID and in the suffix.
    #[test]
    fn test_catalog_multibyte_idseparator_in_id_and_suffix() {
        let scope = catalog("idseparator_multibyte.adoc");

        assert!(scope.lookup("_fooébar").is_some());
        assert!(scope.lookup("_fooébaré2").is_some());
        assert!(scope.lookup("_fooéabar").is_none());
        assert!(scope.lookup("_fooébaréa2").is_none());
    }

    /// Asciidoctor: an empty `:idseparator:` removes only spaces; `-`, `.`
    /// and `_` of the title stay in the ID.
    #[test]
    fn test_catalog_empty_idseparator_keeps_dash_dot_underscore() {
        let scope = catalog("idseparator_empty_punct.adoc");

        assert!(scope.lookup("_foo-barbaz").is_some());
        assert!(scope.lookup("_foo_barqux").is_some());
        assert!(scope.lookup("_foo.barzap").is_some());
        assert!(scope.lookup("_foobarbaz").is_none());
    }

    /// A file included twice with `tag`/`lines` is the usual way to read two
    /// regions of it. The walk knows no regions, so it reports no duplicate
    /// (Asciidoctor reports none for disjoint regions either).
    #[test]
    fn test_catalog_file_included_twice_with_region_is_no_duplicate() {
        let scope = catalog("twice_tagged.adoc");

        assert!(scope.duplicates().is_empty());
        assert_eq!(file_name(scope.lookup("in-a").unwrap()), "tagged.adoc");
    }

    /// Pins the section branch of the region exemption: the section inside
    /// `tag::a` of `tagged.adoc` is read once, no `_in_a_2`.
    #[test]
    fn test_catalog_section_in_file_included_twice_with_region_gets_no_suffix() {
        let scope = catalog("twice_tagged.adoc");

        assert!(scope.lookup("_in_a").is_some());
        assert!(scope.lookup("_in_a_2").is_none());
    }

    /// Asciidoctor reports only `in-a` when a tagged and a plain include of
    /// the same file meet; `outside` is read once and `_sec_out_2` is no ID.
    /// The walk knows no regions, so it reports nothing, in either order.
    #[test]
    fn test_catalog_tagged_then_plain_include_has_no_duplicate_outside_tag() {
        let scope = catalog("tagged_then_plain.adoc");

        assert!(scope.duplicates().is_empty());
        assert!(scope.lookup("_sec_out_2").is_none());
        assert!(scope.lookup("_sec_out").is_some());
    }

    /// Trade-off: Asciidoctor would resolve `_sec_a_2` for the tagged
    /// reading of `Sec A`, the catalog drops it.
    #[test]
    fn test_catalog_plain_then_tagged_include_has_no_duplicate_outside_tag() {
        let scope = catalog("plain_then_tagged.adoc");

        assert!(scope.lookup("_sec_a_2").is_none());
        assert!(scope.duplicates().is_empty());
        assert!(scope.lookup("_sec_out_2").is_none());
        assert!(scope.lookup("_sec_out").is_some());
    }

    /// The header title of the top-level document gets no ID, the level-0
    /// title of an included file does; read as top-level it gets none.
    #[test]
    fn test_catalog_doctitle_gets_no_id_included_title_does() {
        let scope = catalog("title_included.adoc");

        assert!(scope.lookup("_doc").is_none());
        assert!(scope.lookup("_title").is_some());

        let root = fixture();
        let part = root.join("title_part.adoc");
        let ws = Workspace::build(&root, &root.join("title_included.adoc"));
        assert!(ws.anchor_scope_of(&part).lookup("_title").is_none());
    }

    /// Without a region the whole file is read twice and Asciidoctor warns
    /// (`id assigned to block already in use: shared`), so the catalog
    /// reports the second reading.
    #[test]
    fn test_catalog_file_included_twice_without_region_is_duplicate() {
        let scope = catalog("twice_plain.adoc");

        assert_eq!(scope.duplicates().len(), 1);
        let duplicate = &scope.duplicates()[0];
        assert_eq!(duplicate.first.line, duplicate.second.line);
        assert_eq!(file_name(&duplicate.second), "dup_z.adoc");
    }

    /// Asciidoctor reports nothing: `[[x]]` of the snippet lies outside
    /// `tag=t`. The walk knows no regions, so a real collision inside a tag
    /// is missed (false negative); a false warning is worse.
    #[test]
    fn test_catalog_id_outside_tag_read_by_region_include_is_no_duplicate_false_negative() {
        let scope = catalog("region_outside_main.adoc");

        assert!(scope.duplicates().is_empty());
    }

    /// Asciidoctor warns once at the second plain include
    /// (`id assigned to block already in use`), a tagged include before them
    /// changes nothing.
    #[test]
    fn test_catalog_tagged_then_two_plain_includes_is_one_duplicate() {
        let scope = catalog("tagged_then_plain_twice.adoc");

        assert_eq!(scope.duplicates().len(), 1);
        assert_eq!(file_name(&scope.duplicates()[0].second), "dup_z.adoc");
    }

    /// Asciidoctor: the tagged include reads `Sec A` only, the first plain
    /// one takes `_sec_out`, the second `_sec_out_2`. The catalog registers
    /// `_sec_out` at the tagged reading, drops the first plain one and
    /// suffixes the second.
    #[test]
    fn test_catalog_tagged_then_two_plain_includes_suffixes_section_once() {
        let scope = catalog("tagged_then_plain_twice_sec.adoc");

        assert!(scope.lookup("_sec_out").is_some());
        assert!(scope.lookup("_sec_out_2").is_some());
        assert!(scope.lookup("_sec_out_3").is_none());
    }

    /// Fails if `leave` does not close a region: the plain includes after
    /// the region include must still count as plain readings.
    #[test]
    fn test_catalog_region_depth_is_balanced_after_region_include() {
        let scope = catalog("region_then_plain_twice.adoc");

        assert_eq!(scope.duplicates().len(), 1);
    }

    /// The ids of the duplicates of `anchor_same_line/<entry>`, in report
    /// order. `part.adoc` defines `[[p]]` and `[[q]]` on its line 5, outside
    /// `tag::a`.
    fn same_line_duplicates(entry: &str) -> Vec<String> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/anchor_same_line")
            .canonicalize()
            .unwrap();
        let document = root.join(entry);
        let ws = Workspace::build(&root, &document);
        ws.anchor_scope_of(&document)
            .duplicates()
            .iter()
            .map(|duplicate| duplicate.id.clone())
            .collect()
    }

    /// Asciidoctor: no message. The plain reading is the first of line 5.
    #[test]
    fn test_catalog_region_then_plain_two_anchors_on_one_line_is_no_duplicate() {
        assert_eq!(
            same_line_duplicates("region_then_plain.adoc"),
            Vec::<String>::new()
        );
    }

    /// Asciidoctor: one warning for `p`, one for `q`, at the third include.
    #[test]
    fn test_catalog_region_then_two_plain_includes_two_anchors_one_duplicate_each() {
        assert_eq!(
            same_line_duplicates("region_then_two_plain.adoc"),
            ["p", "q"]
        );
    }

    /// Asciidoctor: one warning for `p`, one for `q`, at the second include.
    #[test]
    fn test_catalog_plain_twice_two_anchors_on_one_line_two_duplicates() {
        assert_eq!(same_line_duplicates("plain_twice.adoc"), ["p", "q"]);
    }

    fn selection_edge(attributes: &[(&str, &str)]) -> IncludeEdge {
        IncludeEdge {
            target: PathBuf::from("/a.adoc"),
            line: 1,
            attributes: attributes
                .iter()
                .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                .collect(),
            placement: Placement::Body,
        }
    }

    /// The classes come from 39 Asciidoctor 2.0.26 probes (`xref:a.adoc#id[]`
    /// renders as `#id` for a non-partial include, as `a.html#id` for a
    /// partial one); the first key present of `lines`, `tag`, `tags` decides.
    #[rstest::rstest]
    #[case::none(&[], IncludeSelection::Whole)]
    #[case::leveloffset(&[("leveloffset", "+1")], IncludeSelection::Whole)]
    #[case::all_tags(&[("tags", "**")], IncludeSelection::Whole)]
    #[case::all_tag(&[("tag", "**")], IncludeSelection::Whole)]
    #[case::all_and_untagged(&[("tags", "**;*")], IncludeSelection::Whole)]
    #[case::untagged_and_all(&[("tags", "*;**")], IncludeSelection::Whole)]
    #[case::empty_tags(&[("tags", "")], IncludeSelection::Whole)]
    #[case::bare_negation_tag(&[("tag", "!")], IncludeSelection::Whole)]
    #[case::bare_negation_tags(&[("tags", "!")], IncludeSelection::Whole)]
    #[case::empty_lines(&[("lines", "")], IncludeSelection::Whole)]
    #[case::empty_lines_before_tag(&[("lines", ""), ("tag", "t")], IncludeSelection::Whole)]
    #[case::empty_tag_before_tags(&[("tag", ""), ("tags", "t")], IncludeSelection::Whole)]
    #[case::all_but_one(&[("tags", "**;!t")], IncludeSelection::AllButTags)]
    #[case::all_but_unknown(&[("tags", "**;!nope")], IncludeSelection::AllButTags)]
    #[case::all_but_two(&[("tags", "**;!t;!u")], IncludeSelection::AllButTags)]
    #[case::all_but_comma(&[("tags", "**,!t")], IncludeSelection::AllButTags)]
    #[case::negation_first(&[("tags", "!t;**")], IncludeSelection::AllButTags)]
    #[case::only_negation_tags(&[("tags", "!t")], IncludeSelection::AllButTags)]
    #[case::only_negation_tag(&[("tag", "!t")], IncludeSelection::AllButTags)]
    #[case::only_negations(&[("tags", "!t;!u")], IncludeSelection::AllButTags)]
    #[case::untagged_only(&[("tags", "*")], IncludeSelection::Part)]
    #[case::untagged_only_tag(&[("tag", "*")], IncludeSelection::Part)]
    #[case::untagged_not_t(&[("tags", "*;!t")], IncludeSelection::Part)]
    #[case::not_t_untagged(&[("tags", "!t;*")], IncludeSelection::Part)]
    #[case::t_not_everything(&[("tags", "t;!*")], IncludeSelection::Part)]
    #[case::tag(&[("tag", "t")], IncludeSelection::Part)]
    #[case::tags(&[("tags", "t;u")], IncludeSelection::Part)]
    #[case::unknown_tag(&[("tag", "nope")], IncludeSelection::Part)]
    #[case::all_and_named(&[("tags", "**;t")], IncludeSelection::Part)]
    #[case::named_and_all(&[("tags", "t;**")], IncludeSelection::Part)]
    #[case::not_everything(&[("tags", "!*")], IncludeSelection::Part)]
    #[case::all_but_everything(&[("tags", "**;!*")], IncludeSelection::Part)]
    #[case::not_all(&[("tags", "!**")], IncludeSelection::Part)]
    #[case::not_all_not_t(&[("tags", "!**;!t")], IncludeSelection::Part)]
    #[case::lines_open(&[("lines", "1..-1")], IncludeSelection::Part)]
    #[case::lines_to_end(&[("lines", "1..")], IncludeSelection::Part)]
    #[case::lines_range(&[("lines", "1..3")], IncludeSelection::Part)]
    #[case::lines_garbage(&[("lines", "abc")], IncludeSelection::Part)]
    #[case::tag_before_tags(&[("tag", "t"), ("tags", "**")], IncludeSelection::Part)]
    #[case::tags_before_tag(&[("tags", "**"), ("tag", "t")], IncludeSelection::Part)]
    #[case::lines_before_tags(&[("tags", "**"), ("lines", "1..-1")], IncludeSelection::Part)]
    fn test_include_selection_matches_asciidoctor_partial_rule(
        #[case] attributes: &[(&str, &str)],
        #[case] expected: IncludeSelection,
    ) {
        assert_eq!(include_selection(&selection_edge(attributes)), expected);
    }

    /// `a.adoc` of the tempdir document `main.adoc`, included with `attributes`.
    fn part_of_document(attributes: &str) -> bool {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let main = root.join("main.adoc");
        let part = root.join("a.adoc");
        std::fs::write(&main, format!("= Doc\n\ninclude::a.adoc[{attributes}]\n")).unwrap();
        std::fs::write(&part, "// tag::t[]\n[[x]]\nText.\n// end::t[]\n").unwrap();
        let ws = Workspace::build(&root, &main);
        ws.anchor_scope_of(&main).contains_file(&part)
    }

    /// A selection Asciidoctor does not treat as partial keeps the file part
    /// of the document, so `xref:a.adoc#id[]` stays internal.
    #[rstest::rstest]
    #[case("tags=**")]
    #[case("tag=**")]
    #[case("tags=**;!t")]
    #[case("tags=!t")]
    #[case("tags=")]
    #[case("lines=")]
    #[case("leveloffset=+1")]
    fn test_catalog_file_part_of_document_for_non_partial_tag_selection(#[case] attributes: &str) {
        assert!(part_of_document(attributes));
    }

    #[rstest::rstest]
    #[case("tags=*")]
    #[case("tag=t")]
    #[case("tags=**;t")]
    #[case("tags=!*")]
    #[case("lines=1..-1")]
    #[case("tag=t,tags=**")]
    fn test_catalog_file_not_part_of_document_for_partial_selection(#[case] attributes: &str) {
        assert!(!part_of_document(attributes));
    }

    /// `a.adoc` (`[[p]]` outside its tags) included twice with `attributes`.
    /// Asciidoctor warns once for `leveloffset` and `tags=**`, never for a
    /// partial selection (the walk cannot tell the regions apart).
    #[rstest::rstest]
    #[case("tag=a", 0)]
    #[case("tags=a;b", 0)]
    #[case("lines=1..6", 0)]
    #[case("leveloffset=+1", 1)]
    #[case("leveloffset=-1", 1)]
    #[case("tags=**", 1)]
    fn test_catalog_file_included_twice_reports_duplicates_by_selection(
        #[case] attributes: &str,
        #[case] expected: usize,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let main = root.join("main.adoc");
        std::fs::write(
            &main,
            format!("= Doc\n\ninclude::a.adoc[{attributes}]\n\ninclude::a.adoc[{attributes}]\n"),
        )
        .unwrap();
        std::fs::write(
            root.join("a.adoc"),
            "[[p]]\nOutside.\n\n// tag::a[]\nIn a.\n// end::a[]\n// tag::b[]\nIn b.\n// end::b[]\n",
        )
        .unwrap();
        let ws = Workspace::build(&root, &main);

        assert_eq!(ws.anchor_scope_of(&main).duplicates().len(), expected);
    }

    #[test]
    fn test_anchor_scopes_at_several_top_level_documents_in_path_order() {
        let root = fixture();
        let mut ws = Workspace::build(&root, &root.join("root_one.adoc"));
        ws.scan_workspace();

        let scopes = ws.anchor_scopes_at(&root.join("multi_part.adoc"));

        let documents: Vec<_> = scopes
            .iter()
            .map(|scope| scope.document().file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(documents, ["root_one.adoc", "root_two.adoc"]);
    }

    /// The catalogs are read when asked: a file that loses its definition of
    /// an ID leaves the one a later file has, with no rebuild in between.
    #[test]
    fn test_update_file_dropping_a_definition_leaves_the_other_one() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let book = root.join("book.adoc");
        let setup = root.join("setup.adoc");
        let reference = root.join("reference.adoc");
        std::fs::write(
            &book,
            "= Book\n\ninclude::setup.adoc[]\n\ninclude::reference.adoc[]\n",
        )
        .unwrap();
        std::fs::write(&setup, "[[shared]]\n== Setup\n").unwrap();
        std::fs::write(&reference, "[[shared]]\n== Reference\n").unwrap();
        let mut ws = Workspace::build(&root, &book);
        assert_eq!(
            ws.anchor_scope_of(&book).lookup("shared").unwrap().file,
            setup
        );
        assert_eq!(ws.anchor_scope_of(&book).duplicates().len(), 1);

        std::fs::write(&setup, "== Setup\n").unwrap();
        ws.update_file(&setup, &HashMap::new());

        let scope = ws.anchor_scope_of(&book);
        assert_eq!(scope.lookup("shared").unwrap().file, reference);
        assert!(scope.duplicates().is_empty());
        let shared: Vec<_> = ws
            .anchor_definitions()
            .into_iter()
            .filter(|(id, _)| id == "shared")
            .collect();
        assert_eq!(shared.len(), 1);
        assert_eq!(shared[0].1.file, reference);
    }

    // --- Path resolution of `file#id` targets, ported from the global index ---

    fn xref(target: &str) -> Xref {
        Xref {
            target: target.into(),
            display_text: None,
            line: 1,
            span: None,
            target_col: 0,
        }
    }

    /// A workspace of `source` and `files` (path, anchor id, line) with no includes,
    /// for resolving `target` as written in `source`.
    fn resolve_from(
        registry: crate::attributes::AttributeRegistry,
        source: &Path,
        files: &[(&Path, &str, usize)],
        target: &str,
    ) -> Result<ResolvedXref, DiagnosticKind> {
        let mut documents = HashMap::new();
        documents.insert(
            source.to_path_buf(),
            DocumentModel::test(source.to_str().unwrap()),
        );
        for (path, id, line) in files {
            documents.insert(
                path.to_path_buf(),
                DocumentModel::test(path.to_str().unwrap()).with_anchors(vec![Anchor {
                    id: (*id).into(),
                    reftext: None,
                    line: *line,
                }]),
            );
        }
        let ws = Workspace::new_with(
            PathBuf::from("/"),
            registry,
            crate::graph::IncludeGraph::new(),
            documents,
            HashMap::new(),
        );
        let scope = ws.anchor_scope_of(source);
        CatalogCache::new(&ws).resolve_xref_in(&scope, &xref(target), |file| {
            crate::paths::resolve_doc_target(source, file, &ws.attributes)
        })
    }

    /// A file outside the document is a link to a document of its own and is
    /// checked against that file's catalog.
    #[test]
    fn test_resolve_xref_in_file_anchor_of_foreign_document() {
        let registry = crate::attributes::AttributeRegistry::new(Path::new("/tmp"));
        let file = Path::new("/file.adoc");

        let resolved = resolve_from(
            registry,
            Path::new("/src.adoc"),
            &[(file, "anchor", 42)],
            "file.adoc#anchor",
        )
        .unwrap();

        assert_eq!(resolved.file, file);
        assert_eq!(resolved.line, 42);
    }

    #[test]
    fn test_resolve_xref_in_file_without_anchor_is_line_one() {
        let registry = crate::attributes::AttributeRegistry::new(Path::new("/tmp"));
        let resolved = resolve_from(
            registry,
            Path::new("/src.adoc"),
            &[(Path::new("/target.adoc"), "x", 9)],
            "target.adoc",
        )
        .unwrap();

        assert_eq!(resolved.file, Path::new("/target.adoc"));
        assert_eq!(resolved.line, 1);
    }

    #[test]
    fn test_resolve_xref_in_unknown_bare_id_is_broken() {
        let registry = crate::attributes::AttributeRegistry::new(Path::new("/tmp"));

        let result = resolve_from(registry, Path::new("/src.adoc"), &[], "nonexistent-id");

        assert!(matches!(result, Err(DiagnosticKind::BrokenXref { .. })));
    }

    /// `xref:../b/doc.adoc#id[]` must resolve against the source's folder.
    #[test]
    fn test_resolve_xref_in_sibling_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        let source = root.join("a/src.adoc");
        let target = root.join("b/doc.adoc");
        std::fs::write(&source, "").unwrap();
        std::fs::write(&target, "").unwrap();

        let registry = crate::attributes::AttributeRegistry::new(&root);
        let resolved = resolve_from(
            registry,
            &source,
            &[(&target, "my-anchor", 7)],
            "../b/doc.adoc#my-anchor",
        )
        .unwrap();

        assert_eq!(resolved.file, target);
        assert_eq!(resolved.line, 7);
    }

    /// `xref:{docdir}/sub/doc.adoc#x[]` substitutes `{docdir}` first.
    #[test]
    fn test_resolve_xref_in_attribute_substituted_target() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let source = root.join("index.adoc");
        let target = root.join("sub/doc.adoc");
        std::fs::write(&source, "").unwrap();
        std::fs::write(&target, "").unwrap();

        let registry = crate::attributes::AttributeRegistry::new(&root);
        let resolved = resolve_from(
            registry,
            &source,
            &[(&target, "x", 3)],
            "{docdir}/sub/doc.adoc#x",
        )
        .unwrap();

        assert_eq!(resolved.file, target);
        assert_eq!(resolved.line, 3);
    }

    /// The caller decides where the file part of an xref points (one
    /// attribute scope, ADR-4); the ID lookup stays the same.
    #[test]
    fn test_resolve_xref_in_with_caller_resolved_file() {
        let registry = crate::attributes::AttributeRegistry::new(Path::new("/tmp"));
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/web/doc.adoc"),
            DocumentModel::test("/web/doc.adoc").with_anchors(vec![Anchor {
                id: "x".into(),
                reftext: None,
                line: 5,
            }]),
        );
        documents.insert(PathBuf::from("/src.adoc"), DocumentModel::test("/src.adoc"));
        let ws = Workspace::new_with(
            PathBuf::from("/"),
            registry,
            crate::graph::IncludeGraph::new(),
            documents,
            HashMap::new(),
        );
        let scope = ws.anchor_scope_of(Path::new("/src.adoc"));

        let resolved = CatalogCache::new(&ws)
            .resolve_xref_in(&scope, &xref("{edition}/doc.adoc#x"), |file| {
                (file == "{edition}/doc.adoc").then(|| PathBuf::from("/web/doc.adoc"))
            })
            .unwrap();

        assert_eq!(resolved.file, PathBuf::from("/web/doc.adoc"));
        assert_eq!(resolved.line, 5);
    }

    fn titles_workspace() -> (Workspace, PathBuf) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/xref_completion_titles")
            .canonicalize()
            .unwrap();
        let index = root.join("index.adoc");
        (Workspace::build(&root, &index), index)
    }

    #[test]
    fn test_anchor_title_names_the_section_that_carries_the_id() {
        let (ws, index) = titles_workspace();
        let scope = ws.anchor_scope_of(&index);
        for (id, title) in [
            ("top", "Product Manual"),
            ("intro", "Introduction"),
            ("_audience", "Audience"),
            ("y", "Stacked Run"),
            ("a", "Both Anchors"),
            ("tid", "Inline Anchor"),
            ("_later", "Later"),
        ] {
            let location = scope.lookup(id).unwrap();
            assert_eq!(ws.anchor_title(id, location), Some(title), "{id}");
        }
    }

    #[test]
    fn test_anchor_title_is_none_outside_a_section_title() {
        let (ws, index) = titles_workspace();
        let scope = ws.anchor_scope_of(&index);
        // A paragraph anchor must not take the title of the section after it.
        let para = scope.lookup("para").unwrap();
        assert_eq!(ws.anchor_title("para", para), None);
        // Asciidoctor drops these IDs of a run (`b` loses to `[[a]]`, `x` to
        // `[#y]`): they name no section.
        for (id, line) in [("b", 14), ("x", 9)] {
            let location = AnchorLocation {
                file: index.clone(),
                line,
                kind: AnchorKind::Explicit,
            };
            assert_eq!(ws.anchor_title(id, &location), None, "{id}");
        }
    }
}
