//! Asciidoctor's section sequence check, per top-level document: a title
//! deeper than one level below the section (or document header) above it is
//! out of sequence, and a level 0 title below the header needs doctype
//! `book`. Levels are effective ones, with `leveloffset` applied in reading
//! order.
//!
//! The walk deliberately knows less than Asciidoctor, and then stays quiet
//! instead of guessing:
//! - A document without a header title leaves its topmost sections unchecked
//!   (Asciidoctor checks them when a preamble block comes first; the LSP sees
//!   no blocks before the first title).
//! - Sections read from a part of a file (`tag`, `lines`) or from a part the
//!   grammar could not parse are unknown: they hide their parent from the
//!   check until a section of the same or a shallower level closes them.
//! - An include whose target the LSP does not read (missing, a path with a
//!   build attribute, not in the index) is unknown the same way, from level 1
//!   on.
//! - A `leveloffset` the walk cannot evaluate stops the check for the rest of
//!   the document.

use std::path::{Path, PathBuf};

use crate::anchor_scope::{include_selection, IncludeSelection};
use crate::graph::next_leveloffset;
use crate::reading::{read_in_order, ReadingVisitor};
use crate::types::{AttributeDef, DocumentModel, IncludeEdge, Placement, Section};
use crate::workspace::{entry_name, Workspace};

/// A section title deeper than Asciidoctor expects at that place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutOfSequence {
    pub file: PathBuf,
    pub line: usize,
    /// The levels accepted there, ascending: `[n]`, or `[0, 1]` at the top of
    /// a book.
    pub expected: Vec<usize>,
    pub got: usize,
}

/// A level 0 section title below the document header of a document whose
/// doctype is not `book`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LevelZeroOutsideBook {
    pub file: PathBuf,
    pub line: usize,
}

/// The titles of `document`, read in order with its includes, that are out of
/// sequence.
pub(crate) fn out_of_sequence(workspace: &Workspace, document: &Path) -> Vec<OutOfSequence> {
    walk(workspace, document).found
}

/// The level 0 titles of `document`, read in order with its includes, that
/// Asciidoctor rejects because the doctype is not `book`.
pub(crate) fn level_zero_outside_book(
    workspace: &Workspace,
    document: &Path,
) -> Vec<LevelZeroOutsideBook> {
    walk(workspace, document).level_zero
}

fn walk<'w>(workspace: &'w Workspace, document: &Path) -> SectionWalk<'w> {
    let doctype = workspace
        .attributes
        .preset(document, "doctype")
        .map(|(_, hit)| hit.value);
    let mut walk = SectionWalk {
        workspace,
        offset: 0,
        saved_offsets: Vec::new(),
        doctype,
        region_depth: 0,
        region_min: None,
        block_depth: 0,
        stopped: false,
        frames: Vec::new(),
        found: Vec::new(),
        level_zero: Vec::new(),
    };
    read_in_order(workspace, document, &mut walk);
    walk
}

/// What the walk knows about the sections above the current line.
enum Frame {
    /// The document header: accepts level 1, in a book levels 0 and 1. Never
    /// popped.
    Document,
    /// A section; `expected` is `None` below a special level-1 section.
    Section {
        level: usize,
        expected: Option<usize>,
    },
    /// Sections the walk cannot see may stand here, none shallower than `min`.
    Unknown { min: usize },
}

/// A line of a file the walk acts on.
enum Item<'a> {
    Entry(&'a AttributeDef),
    Section(&'a Section),
}

struct SectionWalk<'w> {
    workspace: &'w Workspace,
    /// The `leveloffset` in force.
    offset: i32,
    /// One entry per include being read: the offset to restore after it.
    saved_offsets: Vec<Option<i32>>,
    doctype: Option<String>,
    /// Includes that read a part of their file, or sit in a part the grammar
    /// could not parse, between the document and the file being read.
    region_depth: usize,
    /// The shallowest section met while `region_depth > 0`.
    region_min: Option<usize>,
    /// Includes inside a block between the document and the file being read:
    /// their content is block content, not sections.
    block_depth: usize,
    /// The levels can no longer be told: no more checks.
    stopped: bool,
    frames: Vec<Frame>,
    found: Vec<OutOfSequence>,
    level_zero: Vec<LevelZeroOutsideBook>,
}

/// Whether the include reads only part of its file or stands where the
/// grammar failed.
fn reads_region(edge: &IncludeEdge) -> bool {
    include_selection(edge) != IncludeSelection::Whole || edge.placement == Placement::Unparsed
}

impl SectionWalk<'_> {
    /// A section of `level` ends every section of that level or deeper, and
    /// every unknown region that starts at `level` or deeper.
    fn close(&mut self, level: usize) {
        while let Some(top) = self.frames.last() {
            let closed = match top {
                Frame::Section { level: open, .. } => *open >= level,
                Frame::Unknown { min } => *min >= level,
                Frame::Document => false,
            };
            if !closed {
                break;
            }
            self.frames.pop();
        }
    }

    /// Sections of a depth of at least `min` may have been read where the
    /// walk could not see them. One of them would have closed every frame of
    /// its depth or deeper, so a following section of `min` or less closes it
    /// in any case (exact); a deeper one has an unknown parent (unchecked).
    fn uncertain(&mut self, min: usize) {
        self.close(min);
        self.frames.push(Frame::Unknown { min });
    }

    fn entry(&mut self, def: &AttributeDef) {
        let (name, unset) = entry_name(&def.name);
        match name {
            "doctype" => self.doctype = (!unset).then(|| def.value.clone()),
            "leveloffset" => {
                if self.region_depth > 0 || self.block_depth > 0 || def.value.contains('{') {
                    self.stopped = true;
                } else if unset {
                    self.offset = 0;
                } else {
                    self.offset = next_leveloffset(self.offset, &def.value);
                }
            }
            _ => {}
        }
    }

    fn section(&mut self, stack_len: usize, file: &Path, section: &Section) {
        if matches!(section.style.as_deref(), Some("discrete" | "float")) {
            return;
        }
        if section.placement == Placement::Block || self.block_depth > 0 {
            return;
        }
        if section.is_header && section.level == 0 && stack_len == 1 {
            self.frames.push(Frame::Document);
            return;
        }
        if self.stopped {
            return;
        }
        let shifted = i32::try_from(section.level).unwrap_or(i32::MAX) + self.offset;
        // A negative level behaves like level 0.
        let level = usize::try_from(shifted).unwrap_or(0);
        if self.region_depth > 0 {
            self.region_min = Some(self.region_min.map_or(level, |min| min.min(level)));
            return;
        }
        if section.placement == Placement::Unparsed {
            self.uncertain(level);
            return;
        }
        // Asciidoctor accepts a level 0 title in a document without a header
        // when a section comes first; the LSP cannot tell whether a block
        // came first, so only documents with a header are checked.
        if level == 0
            && self.doctype.as_deref() != Some("book")
            && matches!(self.frames.first(), Some(Frame::Document))
        {
            self.level_zero.push(LevelZeroOutsideBook {
                file: file.to_path_buf(),
                line: section.line,
            });
        }
        self.close(level);
        let expected = match self.frames.last() {
            Some(Frame::Document) if self.doctype.as_deref() == Some("book") => vec![0, 1],
            Some(Frame::Document) => vec![1],
            Some(Frame::Section {
                expected: Some(next),
                ..
            }) => vec![*next],
            _ => Vec::new(),
        };
        if expected.last().is_some_and(|deepest| level > *deepest) {
            self.found.push(OutOfSequence {
                file: file.to_path_buf(),
                line: section.line,
                expected,
                got: level,
            });
        }
        // Level 1 sections with a style other than the three that take
        // subsections do not support nested ones: Asciidoctor reports that
        // on its own, and their children are not checked here.
        let closed_to_children = level == 1
            && section
                .style
                .as_deref()
                .is_some_and(|style| !matches!(style, "appendix" | "preface" | "abstract"));
        self.frames.push(Frame::Section {
            level,
            expected: (!closed_to_children).then_some(level + 1),
        });
    }
}

impl<'a> ReadingVisitor<'a> for SectionWalk<'_> {
    type Item = Item<'a>;

    fn items(&self, _file: &Path, doc: &'a DocumentModel) -> Vec<(usize, Item<'a>)> {
        doc.attributes
            .iter()
            .map(|def| (def.line, Item::Entry(def)))
            .chain(
                doc.sections
                    .iter()
                    .map(|section| (section.line, Item::Section(section))),
            )
            .collect()
    }

    fn item(&mut self, stack: &[PathBuf], file: &Path, item: Item<'a>) {
        match item {
            Item::Entry(def) => self.entry(def),
            Item::Section(section) => self.section(stack.len(), file, section),
        }
    }

    fn enter(&mut self, edge: &IncludeEdge) {
        let offset = edge
            .attributes
            .iter()
            .find(|(name, _)| name == "leveloffset")
            .map(|(_, value)| value.trim().trim_matches('"'));
        match offset {
            Some(value) if value.contains('{') => {
                self.stopped = true;
                self.saved_offsets.push(None);
            }
            Some(value) => {
                self.saved_offsets.push(Some(self.offset));
                self.offset = next_leveloffset(self.offset, value);
            }
            None => self.saved_offsets.push(None),
        }
        if edge.placement == Placement::Block {
            self.block_depth += 1;
        }
        if reads_region(edge) {
            self.region_depth += 1;
        }
    }

    fn leave(&mut self, edge: &IncludeEdge) {
        // A target the LSP did not read (missing, a path with a build
        // attribute, not indexed) may hold sections. Titles of level 0 and 1
        // are never out of sequence, so one that follows closes the unknown
        // part exactly; a larger `min` would be wrong (`=== Y` right below
        // the header, target `== X`).
        if self.block_depth == 0 && !self.workspace.documents.contains_key(&edge.target) {
            self.uncertain(1);
        }
        if let Some(Some(offset)) = self.saved_offsets.pop() {
            self.offset = offset;
        }
        if edge.placement == Placement::Block {
            self.block_depth -= 1;
        }
        if reads_region(edge) {
            self.region_depth -= 1;
            if self.region_depth == 0 {
                if let Some(min) = self.region_min.take() {
                    self.uncertain(min);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attributes::AttributeRegistry;
    use crate::graph::IncludeGraph;
    use std::collections::HashMap;

    fn section(level: usize, line: usize, placement: Placement) -> Section {
        Section {
            level,
            title: "T".into(),
            line,
            id: Some("id".into()),
            is_header: level == 0,
            style: None,
            placement,
        }
    }

    fn body(level: usize, line: usize) -> Section {
        section(level, line, Placement::Body)
    }

    fn entry(name: &str, value: &str, line: usize) -> AttributeDef {
        AttributeDef {
            name: name.into(),
            value: value.into(),
            line,
        }
    }

    fn edge(target: &str, line: usize, offset: Option<&str>, placement: Placement) -> IncludeEdge {
        IncludeEdge {
            target: PathBuf::from(target),
            line,
            attributes: offset
                .map(|value| ("leveloffset".to_string(), value.to_string()))
                .into_iter()
                .collect(),
            placement,
        }
    }

    /// `/a.adoc` is the top-level document; `parts` are the other files with
    /// the edge that includes each from `/a.adoc`.
    fn walk_documents(
        a: DocumentModel,
        parts: Vec<(DocumentModel, IncludeEdge)>,
    ) -> Vec<(String, usize, Vec<usize>, usize)> {
        walk_with_unread(a, parts, vec![])
    }

    /// Like `walk_documents`, with edges from `/a.adoc` whose target has no
    /// document in the workspace (a path the LSP cannot read).
    fn walk_with_unread(
        a: DocumentModel,
        parts: Vec<(DocumentModel, IncludeEdge)>,
        unread: Vec<IncludeEdge>,
    ) -> Vec<(String, usize, Vec<usize>, usize)> {
        let mut graph = IncludeGraph::new();
        let mut documents = HashMap::new();
        for (part, edge) in parts {
            graph.add_edge(Path::new("/a.adoc"), edge);
            documents.insert(part.path.clone(), part);
        }
        for edge in unread {
            graph.add_edge(Path::new("/a.adoc"), edge);
        }
        documents.insert(PathBuf::from("/a.adoc"), a);
        let workspace = Workspace::new_with(
            PathBuf::from("/"),
            AttributeRegistry::new(Path::new("/tmp")),
            graph,
            documents,
            HashMap::new(),
        );
        out_of_sequence(&workspace, Path::new("/a.adoc"))
            .into_iter()
            .map(|found| {
                (
                    found.file.display().to_string(),
                    found.line,
                    found.expected,
                    found.got,
                )
            })
            .collect()
    }

    /// An unparsed title of level 2 may have been a section: the deeper
    /// sections after it have an unknown parent and stay unchecked, a
    /// shallower section closes it and the check holds again.
    #[test]
    fn test_unparsed_title_hides_the_parent_of_deeper_sections_only() {
        let a = DocumentModel::test("/a.adoc").with_sections(vec![
            body(0, 1),
            body(1, 3),
            section(2, 5, Placement::Unparsed),
            body(3, 7),
            body(1, 9),
            body(3, 11),
        ]);

        assert_eq!(
            walk_documents(a, vec![]),
            vec![("/a.adoc".to_string(), 11, vec![2], 3)]
        );
    }

    /// An include where the grammar failed may read anything: what the target
    /// holds is not judged against the section above it.
    #[test]
    fn test_include_in_an_unparsed_region_is_read_as_a_region() {
        let a = DocumentModel::test("/a.adoc").with_sections(vec![body(0, 1), body(1, 3)]);
        let b = DocumentModel::test("/b.adoc").with_sections(vec![body(3, 1)]);

        assert_eq!(
            walk_documents(a, vec![(b, edge("/b.adoc", 5, None, Placement::Unparsed))]),
            vec![]
        );
    }

    /// Asciidoctor (probes with `[appendix]`, `[preface]` and `[abstract]` on
    /// a level 1 title): `expected level 2, got level 3` for a `====` below
    /// it, unlike other styles such as `[glossary]`, which take no nested
    /// sections.
    #[rstest::rstest]
    #[case::appendix("appendix", vec![("/a.adoc".to_string(), 5, vec![2], 3)])]
    #[case::preface("preface", vec![("/a.adoc".to_string(), 5, vec![2], 3)])]
    #[case::abstract_style("abstract", vec![("/a.adoc".to_string(), 5, vec![2], 3)])]
    #[case::glossary("glossary", vec![])]
    fn test_level_1_style_decides_whether_its_children_are_checked(
        #[case] style: &str,
        #[case] expected: Vec<(String, usize, Vec<usize>, usize)>,
    ) {
        let styled = Section {
            style: Some(style.into()),
            ..body(1, 3)
        };
        let a = DocumentModel::test("/a.adoc").with_sections(vec![body(0, 1), styled, body(3, 5)]);

        assert_eq!(walk_documents(a, vec![]), expected);
    }

    /// Asciidoctor (probes): a `[discrete]` or `[float]` title is no section,
    /// so it closes no section above it and the `====` after it is still
    /// judged against the `===`.
    #[rstest::rstest]
    #[case::discrete("discrete")]
    #[case::float("float")]
    fn test_discrete_title_closes_no_section(#[case] style: &str) {
        let discrete = Section {
            style: Some(style.into()),
            ..body(1, 7)
        };
        let a = DocumentModel::test("/a.adoc").with_sections(vec![
            body(0, 1),
            body(1, 3),
            body(2, 5),
            discrete,
            body(3, 9),
        ]);

        assert_eq!(walk_documents(a, vec![]), vec![]);
    }

    /// Asciidoctor (`-a partials=parts`, `parts/x.adoc` is `=== X`): no
    /// warning for the `====` after the include. A target the LSP does not
    /// read may hold sections of any depth, so the parent is unknown.
    #[test]
    fn test_unread_include_target_hides_the_parent_of_deeper_sections() {
        let a =
            DocumentModel::test("/a.adoc").with_sections(vec![body(0, 1), body(1, 3), body(3, 7)]);

        assert_eq!(
            walk_with_unread(
                a,
                vec![],
                vec![edge("/missing.adoc", 5, None, Placement::Body)]
            ),
            vec![]
        );
    }

    /// Asciidoctor (target `== X`, then `=== Y`): quiet. A title of level 1
    /// or less is never out of sequence, so the unknown region starts at 1,
    /// not deeper.
    #[test]
    fn test_unread_include_under_header_leaves_next_title_unchecked() {
        let a = DocumentModel::test("/a.adoc").with_sections(vec![body(0, 1), body(2, 5)]);

        assert_eq!(
            walk_with_unread(
                a,
                vec![],
                vec![edge("/missing.adoc", 3, None, Placement::Body)]
            ),
            vec![]
        );
    }

    /// Asciidoctor (`-a partials=parts`, `parts/x.adoc` holds `=== X` inside
    /// a `tag::t[]`/`end::t[]` region): no warning for the `====` after a
    /// `tag=` include of a target the LSP does not read. Without the region
    /// Asciidoctor warns, so the tag matters. Regression test for the region
    /// edge on an unread target.
    #[test]
    fn test_unread_region_include_hides_the_parent_of_deeper_sections() {
        let mut tagged = edge("/missing.adoc", 5, None, Placement::Body);
        tagged.attributes.push(("tag".into(), "t".into()));
        let a =
            DocumentModel::test("/a.adoc").with_sections(vec![body(0, 1), body(1, 3), body(3, 7)]);

        assert_eq!(walk_with_unread(a, vec![], vec![tagged]), vec![]);
    }

    /// Asciidoctor: a `== B` after the include switches the check on again,
    /// the `==== W` after it is reported.
    #[test]
    fn test_level_1_title_after_unread_include_resumes_the_check() {
        let a = DocumentModel::test("/a.adoc").with_sections(vec![
            body(0, 1),
            body(1, 3),
            body(2, 7),
            body(3, 9),
            body(1, 11),
            body(3, 13),
        ]);

        assert_eq!(
            walk_with_unread(
                a,
                vec![],
                vec![edge("/missing.adoc", 5, None, Placement::Body)]
            ),
            vec![("/a.adoc".to_string(), 13, vec![2], 3)]
        );
    }

    /// Asciidoctor: an include inside a block reads block content, the
    /// `====` after it is reported either way.
    #[test]
    fn test_unread_include_inside_a_block_keeps_the_check() {
        let a =
            DocumentModel::test("/a.adoc").with_sections(vec![body(0, 1), body(1, 3), body(3, 7)]);

        assert_eq!(
            walk_with_unread(
                a,
                vec![],
                vec![edge("/missing.adoc", 5, None, Placement::Block)]
            ),
            vec![("/a.adoc".to_string(), 7, vec![2], 3)]
        );
    }

    /// The LSP does not resolve `{attr}` in a `leveloffset` (adoc-0172): the
    /// levels after it are unknown, so nothing is reported.
    #[test]
    fn test_leveloffset_with_attribute_reference_stops_checks_lsp_does_not_resolve_it() {
        let a = DocumentModel::test("/a.adoc")
            .with_attributes(vec![entry("leveloffset", "{shift}", 2)])
            .with_sections(vec![body(0, 1), body(3, 4)]);

        assert_eq!(walk_documents(a, vec![]), vec![]);
    }

    #[test]
    fn test_leveloffset_attribute_on_include_with_reference_stops_checks() {
        let a = DocumentModel::test("/a.adoc").with_sections(vec![body(0, 1), body(3, 6)]);
        let b = DocumentModel::test("/b.adoc").with_sections(vec![body(1, 1)]);

        assert_eq!(
            walk_documents(
                a,
                vec![(b, edge("/b.adoc", 3, Some("{shift}"), Placement::Body))]
            ),
            vec![]
        );
    }

    /// The offset set in a file read for a part only is unknown: the LSP does
    /// not know whether Asciidoctor reads the entry.
    #[test]
    fn test_leveloffset_entry_read_under_region_include_stops_checks() {
        let mut tagged = edge("/b.adoc", 3, None, Placement::Body);
        tagged.attributes.push(("tag".into(), "t".into()));
        let a = DocumentModel::test("/a.adoc").with_sections(vec![body(0, 1), body(3, 6)]);
        let b = DocumentModel::test("/b.adoc").with_attributes(vec![entry("leveloffset", "+1", 1)]);

        assert_eq!(walk_documents(a, vec![(b, tagged)]), vec![]);
    }

    /// An include inside a block reads block content, `:leveloffset:` there
    /// is no entry.
    #[test]
    fn test_leveloffset_entry_read_under_block_include_stops_checks() {
        let a = DocumentModel::test("/a.adoc").with_sections(vec![body(0, 1), body(3, 6)]);
        let b = DocumentModel::test("/b.adoc").with_attributes(vec![entry("leveloffset", "+1", 1)]);

        assert_eq!(
            walk_documents(a, vec![(b, edge("/b.adoc", 3, None, Placement::Block))]),
            vec![]
        );
    }

    /// Asciidoctor (probe: `leveloffset=-1` on `= Ch` and `=== Deep`): a
    /// negative effective level behaves like level 0, so the next section is
    /// checked against level 0 and its child level 1.
    #[test]
    fn test_negative_level_counts_as_level_0() {
        let a = DocumentModel::test("/a.adoc").with_sections(vec![body(0, 1)]);
        let b = DocumentModel::test("/b.adoc").with_sections(vec![body(0, 1), body(3, 3)]);

        assert_eq!(
            walk_documents(
                a,
                vec![(b, edge("/b.adoc", 3, Some("-1"), Placement::Body))]
            ),
            vec![("/b.adoc".to_string(), 3, vec![1], 2)]
        );
    }

    /// Asciidoctor with `-a doctype=book` (the CLI does not read the config
    /// file): `expected levels 0 or 1, got level 2`.
    #[test]
    fn test_doctype_book_from_config_accepts_levels_0_or_1() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/section_levels_book_config")
            .canonicalize()
            .unwrap();
        let mut workspace = Workspace::new(&root);
        workspace.scan_workspace();

        let found = out_of_sequence(&workspace, &root.join("index.adoc"));

        assert_eq!(
            found
                .iter()
                .map(|f| (f.line, f.expected.clone(), f.got))
                .collect::<Vec<_>>(),
            vec![(3, vec![0, 1], 2)]
        );
    }
}
