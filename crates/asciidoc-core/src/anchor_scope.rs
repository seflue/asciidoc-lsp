//! The IDs one top-level document defines. Asciidoctor
//! knows an ID only within the document it converts: the top-level document
//! and its includes. The first definition wins, a section whose generated ID
//! is taken gets a numeric suffix, and only an explicit ID on a taken name is
//! a duplicate.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::reading::{read_in_order, ReadingVisitor};
use crate::references::generate_section_id;
use crate::types::{Anchor, AnchorKind, AnchorLocation, DocumentModel, IncludeEdge, Section};
use crate::workspace::Workspace;

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
    files: HashSet<PathBuf>,
    ids: HashMap<String, AnchorLocation>,
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

    /// `file` is the top-level document or one of its includes.
    #[must_use]
    pub fn contains_file(&self, file: &Path) -> bool {
        self.files.contains(file)
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

    /// `document` is already an indexed path.
    fn read_anchor_scope(&self, document: &Path) -> AnchorScope {
        let document = document.to_path_buf();
        let mut walk = AnchorWalk {
            workspace: self,
            region_depth: 0,
            scope: AnchorScope {
                document: document.clone(),
                files: HashSet::from([document.clone()]),
                ids: HashMap::new(),
                duplicates: Vec::new(),
            },
            region_defined: Lines::default(),
            plain_read: Lines::default(),
        };
        read_in_order(self, &document, &mut walk);
        walk.scope
    }
}

/// Whether an include reads only part of its file (`tag`, `tags`, `lines`).
fn reads_region(edge: &IncludeEdge) -> bool {
    edge.attributes
        .iter()
        .any(|(name, _)| matches!(name.as_str(), "tag" | "tags" | "lines"))
}

/// Lines of files, looked up by `&Path` without allocating.
#[derive(Default)]
struct Lines(HashMap<PathBuf, HashSet<usize>>);

impl Lines {
    fn contains(&self, file: &Path, line: usize) -> bool {
        self.0.get(file).is_some_and(|lines| lines.contains(&line))
    }

    /// Whether `line` was new.
    fn insert(&mut self, file: &Path, line: usize) -> bool {
        if let Some(lines) = self.0.get_mut(file) {
            return lines.insert(line);
        }
        self.0.insert(file.to_path_buf(), HashSet::from([line]));
        true
    }
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
    /// Lines first defined while `region_depth > 0`. Such a line may lie
    /// outside the region Asciidoctor reads, so a later reading of it is no
    /// duplicate whichever include comes first.
    region_defined: Lines,
    /// Lines met while `region_depth == 0`. Such a reading covers the whole
    /// file, so meeting the line again is a real second reading even when a
    /// region include read it before.
    plain_read: Lines,
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
                let plain_reread = self.note_plain_reading(file, anchor.line);
                self.define_explicit(file, anchor, plain_reread);
            }
            Definition::Section(section) => {
                // The header title of the top-level document gets no ID. An
                // included level-0 title that Asciidoctor would take as the
                // document title (a top-level file without a header of its
                // own) still gets one here: a false negative, accepted.
                let is_document_header = section.is_header && stack.len() == 1;
                if !section.explicit_id && !is_document_header {
                    let plain_reread = self.note_plain_reading(file, section.line);
                    self.define_section(file, section, plain_reread);
                }
            }
        }
    }

    fn enter(&mut self, edge: &IncludeEdge) {
        if self.workspace.documents.contains_key(&edge.target) {
            self.scope.files.insert(edge.target.clone());
        }
        if reads_region(edge) {
            self.region_depth += 1;
        }
    }

    fn leave(&mut self, edge: &IncludeEdge) {
        if reads_region(edge) {
            self.region_depth -= 1;
        }
    }
}

impl AnchorWalk<'_> {
    /// Records a reading of `line` outside any region; true when an earlier
    /// plain reading met it too.
    fn note_plain_reading(&mut self, file: &Path, line: usize) -> bool {
        self.region_depth == 0 && !self.plain_read.insert(file, line)
    }

    /// The current reading may have covered only part of its file, or the
    /// first one did.
    fn may_miss_region(&self, first: &AnchorLocation) -> bool {
        self.region_depth > 0 || self.region_defined.contains(&first.file, first.line)
    }

    /// A reading of `location` that meets an earlier one is ignorable when
    /// either reading may have covered only part of the file and both are
    /// the same line.
    fn is_region_reread(&self, taken: &AnchorLocation, location: &AnchorLocation) -> bool {
        is_same_line(taken, location) && self.may_miss_region(taken)
    }

    fn register(&mut self, id: String, location: AnchorLocation) {
        if self.region_depth > 0 {
            self.region_defined.insert(&location.file, location.line);
        }
        self.scope.ids.insert(id, location);
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
        if !plain_reread && self.may_miss_region(first) {
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
        let separator = registry.resolve(file, "idseparator").unwrap_or("_");
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
            if !plain_reread && self.is_region_reread(taken, &location) {
                return;
            }
            id = format!("{base}{separator}{suffix}");
            suffix += 1;
        }
        self.register(id, location);
    }
}

fn is_same_line(a: &AnchorLocation, b: &AnchorLocation) -> bool {
    a.file == b.file && a.line == b.line
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
