//! The header of a top-level document as Asciidoctor PDF reads it.
//!
//! Asciidoctor PDF takes `pdf-theme`, `pdf-themesdir` and `pdf-fontsdir` in
//! the state the attributes have at the end of the header. The reader
//! (`parser.rb`, `reader.rb`) puts the top-level document and the files it
//! includes into one stream of lines, resolves the conditionals while it
//! does, and ends the header at the first line that no rule of the header
//! takes: a blank line in an included file ends it like one in the document.
//! [`PdfHeader::read`] follows that stream with the lines classified per file
//! ([`HeaderLine`]).
//!
//! What the walk cannot tell it keeps open. A conditional on an attribute it
//! does not know (a build may pass it with `-a`) and `ifeval` are an unknown
//! branch: an entry in it is [`HeaderValue::Uncertain`], and a line in it
//! that ends the header or moves the reader on may or may not be there, so
//! the header may end at that point or go on. Whatever such a header
//! sets afterwards is uncertain too. The checks never report what is
//! uncertain.
//!
//! Limit, a line that classifies differently from the reader: a line is
//! classified on its own, without the conditionals around it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::attributes::{substitute_attributes, AttributeLayer, DOCDIR};
use crate::extractors::classify_single_line;
use crate::preprocessor::BlockStack;
use crate::reading::{read_in_order, ReadingVisitor};
use crate::types::{
    ConditionalKind, ConditionalOperator, Directive, DocumentModel, HeaderEntry, HeaderLine,
    IncludeEdge,
};
use crate::workspace::Workspace;

/// Attributes an Asciidoctor PDF build defines. The converter sets
/// `basebackend` to `html` (`converter.rb:135`), so `basebackend-html` holds
/// and `basebackend-pdf` does not.
const PDF_BACKEND_ATTRIBUTES: &[&str] =
    &["backend", "backend-pdf", "basebackend", "basebackend-html"];

/// Attributes the PDF build leaves unset, however the build is called.
const PDF_UNSET_ATTRIBUTES: &[&str] = &["basebackend-pdf"];

/// An attribute entry of the header: where it stands and its value with the
/// references substituted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub file: PathBuf,
    pub line: usize,
    pub value: String,
}

/// What the header says about one attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HeaderValue {
    Set(Entry),
    Unset,
    /// Set or unset in a place the header may not reach, or by an entry in an
    /// unknown branch.
    Uncertain,
}

static UNCERTAIN: HeaderValue = HeaderValue::Uncertain;

/// The attributes the header of one top-level document sets.
#[derive(Debug)]
pub(crate) struct PdfHeader {
    /// The last entry of each attribute, by lower-case name.
    current: HashMap<String, HeaderValue>,
    /// An include could not be read, or a comment block in it never closes
    /// and goes on in the including file: any attribute may have been set.
    blind: bool,
}

impl PdfHeader {
    /// Read the header of the top-level document `root`.
    pub(crate) fn read(workspace: &Workspace, root: &Path) -> Self {
        let docdir = workspace
            .attributes()
            .resolve(root, DOCDIR)
            .unwrap_or_default()
            .to_string();
        let mut walk = HeaderWalk {
            workspace,
            root,
            docdir,
            current: HashMap::new(),
            branches: BlockStack::new(),
            stages: Stage::BeforeTitle.bit(),
            may_have_ended: false,
            drained: None,
            include_open: false,
            title_taken: Tri::No,
            blind: false,
        };
        read_in_order(workspace, root, &mut walk);
        Self {
            current: walk.current,
            blind: walk.blind,
        }
    }

    /// What the header says about `name` (lower case); `None` when it does
    /// not touch it.
    pub(crate) fn value(&self, name: &str) -> Option<&HeaderValue> {
        if self.blind {
            Some(&UNCERTAIN)
        } else {
            self.current.get(name)
        }
    }
}

/// Where the reader is in the header, `parse_document_header` in
/// `parser.rb`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Before the title: blank lines, comments, entries and block metadata.
    BeforeTitle,
    /// After the title: entries and comments, then the author line.
    AfterTitle,
    /// After the author line: entries and comments, then the revision line.
    AfterAuthor,
    /// After the revision line: entries and comments, then the body.
    AfterRevision,
}

impl Stage {
    const ALL: [Stage; 4] = [
        Stage::BeforeTitle,
        Stage::AfterTitle,
        Stage::AfterAuthor,
        Stage::AfterRevision,
    ];

    fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// A line of the stream that moves the reader, as far as it tells them
/// apart; comments and entries are skipped in every stage.
#[derive(Clone, Copy)]
enum Kind {
    Blank,
    Metadata,
    Title,
    Text,
    /// Text that cannot be a revision line.
    NotRevision,
}

enum Next {
    Stay,
    To(Stage),
    End,
}

fn step(stage: Stage, kind: Kind) -> Next {
    match (stage, kind) {
        (Stage::BeforeTitle, Kind::Blank | Kind::Metadata) => Next::Stay,
        (Stage::BeforeTitle, Kind::Title) => Next::To(Stage::AfterTitle),
        (Stage::BeforeTitle, _) | (Stage::AfterTitle | Stage::AfterAuthor, Kind::Blank) => {
            Next::End
        }
        (Stage::AfterTitle, _) => Next::To(Stage::AfterAuthor),
        (Stage::AfterAuthor, Kind::NotRevision) | (Stage::AfterRevision, _) => Next::End,
        (Stage::AfterAuthor, _) => Next::To(Stage::AfterRevision),
    }
}

/// Whether a block the reader reads is certain.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Branch {
    Active,
    /// The reader drops its lines, includes with them.
    Skipped,
    Unknown,
}

/// Whether an attribute is defined.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tri {
    Yes,
    No,
    Unknown,
}

impl Tri {
    fn not(self) -> Self {
        match self {
            Tri::Yes => Tri::No,
            Tri::No => Tri::Yes,
            Tri::Unknown => Tri::Unknown,
        }
    }
}

struct HeaderWalk<'w> {
    workspace: &'w Workspace,
    root: &'w Path,
    docdir: String,
    current: HashMap<String, HeaderValue>,
    /// The open conditional blocks, each as it applies with the ones around
    /// it. The stack runs across files, as Asciidoctor's does.
    branches: BlockStack<Branch>,
    /// The stages the reader may be in, as [`Stage::bit`]s: more than one
    /// after a line in an unknown branch that may not be there.
    stages: u8,
    /// The header may have ended already.
    may_have_ended: bool,
    /// The depth of the unknown branch whose lines can no longer change what
    /// the header says but make entries uncertain: a line of it ended the
    /// header in every reading in which the branch holds. Its includes are not
    /// read, up to its `endif`.
    drained: Option<usize>,
    /// The line just read is an include the reader follows.
    include_open: bool,
    /// Whether block metadata above the title took it: `.Title`,
    /// `[discrete]`, `[float]`.
    title_taken: Tri,
    /// The reader's place is lost, see [`PdfHeader`].
    blind: bool,
}

impl<'a> ReadingVisitor<'a> for HeaderWalk<'_> {
    type Item = (usize, &'a HeaderLine);

    fn items(&self, _file: &Path, doc: &'a DocumentModel) -> Vec<(usize, Self::Item)> {
        doc.header_lines
            .iter()
            .enumerate()
            .filter(|(_, class)| !matches!(class, HeaderLine::Covered))
            .map(|(index, class)| (index + 1, (index + 1, class)))
            .collect()
    }

    fn item(&mut self, stack: &[PathBuf], file: &Path, (line, class): Self::Item) {
        self.include_open = false;
        let top_level = stack.len() == 1;
        match class {
            HeaderLine::Directive(directive) => self.directive(directive, file, line, top_level),
            HeaderLine::Include => {
                self.include_open = self.branch() != Branch::Skipped && self.drained.is_none();
            }
            _ => {
                let branch = self.branch();
                let over = self.stream_line(branch, class, file, line, top_level);
                if over && branch == Branch::Unknown && self.drained.is_none() {
                    self.drained = Some(self.branches.len());
                }
            }
        }
    }

    fn follow(&mut self, edge: &IncludeEdge) -> bool {
        if !std::mem::take(&mut self.include_open) {
            return false;
        }
        if self.workspace.documents.contains_key(&edge.target) {
            return true;
        }
        self.blind = true;
        false
    }

    fn done(&self) -> bool {
        self.stages == 0 || self.blind
    }
}

impl HeaderWalk<'_> {
    fn branch(&self) -> Branch {
        self.branches.last().copied().unwrap_or(Branch::Active)
    }

    /// How a block with the local branch `local` applies inside the open ones.
    fn nested(&self, local: Branch) -> Branch {
        match (self.branch(), local) {
            (Branch::Skipped, _) | (_, Branch::Skipped) => Branch::Skipped,
            (Branch::Unknown, _) | (_, Branch::Unknown) => Branch::Unknown,
            _ => Branch::Active,
        }
    }

    fn directive(&mut self, directive: &Directive, file: &Path, line: usize, top_level: bool) {
        match directive {
            Directive::Endif { target } => {
                self.branches.close(target.as_deref());
                if self
                    .drained
                    .is_some_and(|depth| self.branches.len() < depth)
                {
                    self.drained = None;
                }
            }
            Directive::Ifeval => {
                let branch = self.nested(Branch::Unknown);
                self.branches.open_ifeval(branch);
            }
            // A `Dropped` line the reader consumes with an error and acts on
            // none of it. An `include` is never classified as a directive
            // (see `HeaderLine::Include`), the arm only completes the match.
            Directive::Include { .. } | Directive::Dropped => {}
            Directive::Condition {
                kind,
                names,
                operator,
                content,
            } => {
                let local = match self.holds(*kind, names, *operator) {
                    Tri::Yes => Branch::Active,
                    Tri::No => Branch::Skipped,
                    Tri::Unknown => Branch::Unknown,
                };
                match content {
                    None => {
                        let branch = self.nested(local);
                        self.branches.open_condition(names, *operator, branch);
                    }
                    Some(text) if self.branch() != Branch::Skipped && local != Branch::Skipped => {
                        let branch = if local == Branch::Unknown {
                            Branch::Unknown
                        } else {
                            self.branch()
                        };
                        self.stream_line(
                            branch,
                            &classify_single_line(text),
                            file,
                            line,
                            top_level,
                        );
                    }
                    Some(_) => {}
                }
            }
        }
    }

    /// Whether the condition of an `ifdef` or `ifndef` holds.
    fn holds(&self, kind: ConditionalKind, names: &[String], operator: ConditionalOperator) -> Tri {
        let defined: Vec<Tri> = names.iter().map(|name| self.defined(name)).collect();
        let all = |state| defined.iter().all(|&have| have == state);
        let any = |state| defined.contains(&state);
        let holds = if operator == ConditionalOperator::And {
            if any(Tri::No) {
                Tri::No
            } else if all(Tri::Yes) {
                Tri::Yes
            } else {
                Tri::Unknown
            }
        } else if any(Tri::Yes) {
            Tri::Yes
        } else if all(Tri::No) {
            Tri::No
        } else {
            Tri::Unknown
        };
        match kind {
            ConditionalKind::Ifdef => holds,
            ConditionalKind::Ifndef => holds.not(),
        }
    }

    /// Whether `name` is defined where the reader stands. An attribute no
    /// entry, configuration or PDF build defines may still come from the
    /// build (`-a name`), so it is not known to be unset.
    fn defined(&self, name: &str) -> Tri {
        match self.current.get(name) {
            Some(HeaderValue::Set(_)) => Tri::Yes,
            Some(HeaderValue::Unset) => Tri::No,
            Some(HeaderValue::Uncertain) => Tri::Unknown,
            None if PDF_BACKEND_ATTRIBUTES.contains(&name) => Tri::Yes,
            None if PDF_UNSET_ATTRIBUTES.contains(&name) => Tri::No,
            None => match self.workspace.attributes().preset(self.root, name) {
                Some((AttributeLayer::Config, _)) => Tri::Yes,
                // The other builtins exist in the editor only.
                Some((AttributeLayer::Builtin, _)) if name == DOCDIR => Tri::Yes,
                _ => Tri::Unknown,
            },
        }
    }

    /// One line of the stream as it stands in `branch`. Whether the header is
    /// over in every reading in which the line is there.
    fn stream_line(
        &mut self,
        branch: Branch,
        class: &HeaderLine,
        file: &Path,
        line: usize,
        top_level: bool,
    ) -> bool {
        if branch == Branch::Skipped {
            return false;
        }
        let uncertain = branch == Branch::Unknown;
        let kinds = match class {
            HeaderLine::Entry(entry) => {
                self.define(entry, file, line, !uncertain && !self.may_have_ended);
                return false;
            }
            HeaderLine::Comment => return false,
            HeaderLine::BlockComment { end } => {
                // Asciidoctor goes on with the comment in the including file.
                self.blind |= end.is_none() && !top_level;
                return false;
            }
            _ if self.drained.is_some() => return false,
            HeaderLine::Blank => &[Kind::Blank][..],
            HeaderLine::Metadata => &[Kind::Metadata][..],
            HeaderLine::TitleBlocker => {
                self.title_taken = match (uncertain, self.title_taken) {
                    (false, _) | (_, Tri::Yes) => Tri::Yes,
                    (true, _) => Tri::Unknown,
                };
                &[Kind::Metadata][..]
            }
            HeaderLine::Title if top_level => match self.title_taken {
                Tri::No => &[Kind::Title][..],
                Tri::Yes => &[Kind::Text][..],
                Tri::Unknown => &[Kind::Title, Kind::Text][..],
            },
            HeaderLine::NotRevision => &[Kind::NotRevision][..],
            _ => &[Kind::Text][..],
        };
        let mut next = 0;
        for (stage, kind) in Stage::ALL
            .into_iter()
            .filter(|stage| self.stages & stage.bit() != 0)
            .flat_map(|stage| kinds.iter().map(move |kind| (stage, *kind)))
        {
            match step(stage, kind) {
                Next::Stay => next |= stage.bit(),
                Next::To(to) => next |= to.bit(),
                Next::End => self.may_have_ended = true,
            }
        }
        // A line that may not be there leaves the reader where it was too.
        self.stages = if uncertain { next | self.stages } else { next };
        next == 0
    }

    fn define(&mut self, entry: &HeaderEntry, file: &Path, line: usize, certain: bool) {
        let value = match (certain, entry.unset) {
            (false, _) => HeaderValue::Uncertain,
            (true, true) => HeaderValue::Unset,
            (true, false) => HeaderValue::Set(Entry {
                file: file.to_path_buf(),
                line,
                value: self.substitute(&entry.value),
            }),
        };
        self.current.insert(entry.name.clone(), value);
    }

    /// `input` with the attributes the header has so far substituted, else
    /// those the top-level document starts with (configuration, builtins).
    /// An unset or uncertain reference stays as written.
    fn substitute(&self, input: &str) -> String {
        substitute_attributes(input, |name| {
            if name == DOCDIR {
                return Some(self.docdir.clone());
            }
            match self.current.get(&name.to_lowercase()) {
                Some(HeaderValue::Set(entry)) => Some(entry.value.clone()),
                Some(HeaderValue::Unset | HeaderValue::Uncertain) => None,
                None => self
                    .workspace
                    .attributes()
                    .preset(self.root, name)
                    .map(|(_, hit)| hit.value),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header of `main.adoc` in a directory with `files`, and how it
    /// sees `pdf-theme`: `set:<value>` (`@file:line` when from another
    /// file), `unset`, `uncertain` or `none`.
    fn theme(files: &[(&str, &str)]) -> String {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for (name, text) in files {
            std::fs::write(root.join(name), text).unwrap();
        }
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        let header = PdfHeader::read(&ws, &root.join("main.adoc"));
        match header.value("pdf-theme") {
            Some(HeaderValue::Set(entry)) => {
                let file = entry.file.file_name().unwrap().to_string_lossy();
                if file == "main.adoc" {
                    format!("set:{}", entry.value)
                } else {
                    format!("set:{}@{file}:{}", entry.value, entry.line)
                }
            }
            Some(HeaderValue::Unset) => "unset".to_string(),
            Some(HeaderValue::Uncertain) => "uncertain".to_string(),
            None => "none".to_string(),
        }
    }

    fn main(text: &str) -> String {
        theme(&[("main.adoc", text)])
    }

    #[test]
    fn the_title_author_and_revision_lines_come_before_the_entries() {
        assert_eq!(main("= T\n:pdf-theme: a\n\nBody.\n"), "set:a");
        assert_eq!(main("= T\nJane\nv1.0\n:pdf-theme: a\n\nBody.\n"), "set:a");
        // The blank line ends the header, so does a fourth line of text.
        assert_eq!(main("= T\n\n:pdf-theme: a\n"), "none");
        assert_eq!(main("= T\nJane\n\n:pdf-theme: a\n"), "none");
        assert_eq!(main("= T\nJane\nv1.0\nMore.\n:pdf-theme: a\n"), "none");
        assert_eq!(main("= T\nJane\nv1.0\n:x: y\n\n:pdf-theme: a\n"), "none");
    }

    #[test]
    fn a_document_without_title_reads_entries_through_blank_lines() {
        assert_eq!(main(":pdf-theme: a\n\n:pdf-theme: b\n\nBody.\n"), "set:b");
        assert_eq!(main(":a: b\nText.\n:pdf-theme: a\n"), "none");
        assert_eq!(main("\n\n[[id]]\n.Title\n:pdf-theme: a\n"), "set:a");
    }

    #[test]
    fn comments_and_continued_entries_do_not_end_the_header() {
        let text = "= T\n// c\n////\nnote\n\n////\n:d: x \\\n  y\n:pdf-theme: a\n";
        assert_eq!(main(text), "set:a");
        // An unterminated comment block reads the rest as comment.
        assert_eq!(main("= T\n////\n:pdf-theme: a\n"), "none");
    }

    /// `RevisionInfoLineRx` rejects a line that starts with a colon unless a
    /// comma is followed by something else than a colon. Asciidoctor throws
    /// the rejected line back and the header ends (oracle: Asciidoctor
    /// 2.0.26, `doc.attr('pdf-theme')` is `nil`).
    #[test]
    fn a_line_after_the_author_that_is_no_revision_line_ends_the_header() {
        assert_eq!(
            main(
                "= T
Jane
:e:f
:pdf-theme: a
"
            ),
            "none"
        );
        assert_eq!(
            main(
                "= T
Jane
:e,:f
:pdf-theme: a
"
            ),
            "none"
        );
        assert_eq!(
            main(
                "= T
Jane
:e:f, x
:pdf-theme: a
"
            ),
            "set:a"
        );
        assert_eq!(
            main(
                "= T
Jane
 :e:f
:pdf-theme: a
"
            ),
            "set:a"
        );
        assert_eq!(
            main(
                "= T
Jane
v1.0
:pdf-theme: a
"
            ),
            "set:a"
        );
    }

    /// A block title or a `discrete` or `float` style above the title keeps
    /// it from being the document title: the header ends there (oracle:
    /// `nil`). Other attributes do not.
    #[test]
    fn block_metadata_that_takes_the_title_ends_the_header() {
        for above in [
            ".Foo\n",
            ".Foo\n\n",
            "[[id]]\n.Foo\n",
            "[discrete]\n",
            "[float,role=x]\n",
            "[discrete#id.role]\n\n",
            ".Foo\n[discrete]\n",
        ] {
            let text = format!("{above}= T\n:pdf-theme: a\n");
            assert_eq!(main(&text), "none", "{above:?}");
        }
        for above in [
            "[role=x]\n",
            "[[id]]\n",
            "[quote]\n",
            "[.role]\n",
            "[%hardbreaks]\n",
        ] {
            let text = format!("{above}= T\n:pdf-theme: a\n");
            assert_eq!(main(&text), "set:a", "{above:?}");
        }
    }

    /// An unterminated comment block in an include goes on in the including
    /// file (oracle: the entry after the include is read as comment). The
    /// place of the reader is lost, so nothing is known.
    #[test]
    fn an_unterminated_comment_in_an_include_leaves_everything_uncertain() {
        let files = [
            ("main.adoc", "= T\ninclude::inc.adoc[]\n:pdf-theme: a\n"),
            ("inc.adoc", "////\nnever closed\n"),
        ];
        assert_eq!(theme(&files), "uncertain");
        let files = [
            ("main.adoc", "= T\ninclude::inc.adoc[]\n:pdf-theme: a\n"),
            ("inc.adoc", "////\nclosed\n////\n"),
        ];
        assert_eq!(theme(&files), "set:a");
    }

    #[test]
    fn a_blank_line_in_an_include_ends_the_header() {
        let files = [
            ("main.adoc", "= T\ninclude::inc.adoc[]\n:pdf-theme: a\n"),
            ("inc.adoc", ":x: y\n\n:z: w\n"),
        ];
        assert_eq!(theme(&files), "none");
        let files = [
            ("main.adoc", "= T\ninclude::inc.adoc[]\n:pdf-theme: a\n"),
            ("inc.adoc", ":x: y\n:pdf-theme: b\n"),
        ];
        assert_eq!(theme(&files), "set:a");
        let files = [
            ("main.adoc", "= T\ninclude::inc.adoc[]\n"),
            ("inc.adoc", ":pdf-theme: b\n"),
        ];
        assert_eq!(theme(&files), "set:b@inc.adoc:1");
    }

    #[test]
    fn a_file_included_twice_is_read_twice() {
        let files = [
            (
                "main.adoc",
                "= T\ninclude::inc.adoc[]\n:pdf-theme: mid\ninclude::inc.adoc[]\n",
            ),
            ("inc.adoc", ":pdf-theme: inc\n"),
        ];
        assert_eq!(theme(&files), "set:inc@inc.adoc:1");
    }

    #[test]
    fn an_include_in_a_branch_that_does_not_hold_is_not_read() {
        let files = [
            (
                "main.adoc",
                "= T\n:nope!:\n:pdf-theme: a\nifdef::nope[]\ninclude::inc.adoc[]\nendif::[]\n",
            ),
            ("inc.adoc", ":pdf-theme: b\n\n"),
        ];
        assert_eq!(theme(&files), "set:a");
        // A branch that holds reads it.
        let files = [
            (
                "main.adoc",
                "= T\n:pdf-theme: a\nifdef::backend-pdf[]\ninclude::inc.adoc[]\nendif::[]\n",
            ),
            ("inc.adoc", ":pdf-theme: b\n"),
        ];
        assert_eq!(theme(&files), "set:b@inc.adoc:1");
    }

    #[test]
    fn the_conditional_stack_runs_across_files() {
        let files = [
            (
                "main.adoc",
                "= T\ninclude::inc.adoc[]\n:pdf-theme: a\nendif::[]\n:pdf-theme: b\n",
            ),
            ("inc.adoc", "ifndef::backend-pdf[]\n"),
        ];
        assert_eq!(theme(&files), "set:b");
    }

    #[test]
    fn a_one_line_conditional_that_holds_is_a_line_of_the_stream() {
        assert_eq!(main("= T\nifdef::backend-pdf[:pdf-theme: a]\n"), "set:a");
        assert_eq!(main("= T\nifndef::backend-pdf[:pdf-theme: a]\n"), "none");
        assert_eq!(
            main("= T\nifdef::backend-pdf[Text]\n:pdf-theme: a\n"),
            "set:a"
        );
        assert_eq!(
            main("= T\nJane\nv1.0\nifdef::backend-pdf[Text]\n:pdf-theme: a\n"),
            "none"
        );
    }

    #[test]
    fn conditions_combine_names_as_asciidoctor_does() {
        let any = "= T\nifdef::nope,backend-pdf[:pdf-theme: a]\n";
        assert_eq!(main(any), "set:a");
        let all = "= T\n:nope!:\nifdef::nope+backend-pdf[:pdf-theme: a]\n";
        assert_eq!(main(all), "none");
        let not_any = "= T\n:nope!:\nifndef::nope,other[]\n:pdf-theme: a\nendif::[]\n";
        assert_eq!(main(not_any), "uncertain");
    }

    #[test]
    fn an_attribute_the_build_may_set_is_not_known_to_be_unset() {
        assert_eq!(
            main("= T\nifndef::foo[]\n:pdf-theme: a\nendif::[]\n"),
            "uncertain"
        );
        assert_eq!(
            main("= T\nifdef::foo[]\n:pdf-theme: a\nendif::[]\n"),
            "uncertain"
        );
        // An entry above settles it.
        assert_eq!(
            main("= T\n:foo:\nifdef::foo[]\n:pdf-theme: a\nendif::[]\n"),
            "set:a"
        );
        // The builtin only the editor sets is no PDF attribute.
        let editor = "= T\nifdef::env-asciidoc-lsp[]\n:pdf-theme: a\nendif::[]\n";
        assert_eq!(main(editor), "uncertain");
    }

    /// An attribute from `.asciidoctorconfig` counts as set, as in the
    /// editor, and a `:corp!:` in the document cancels it.
    #[test]
    fn a_conditional_on_a_config_attribute_follows_the_config() {
        let config = (".asciidoctorconfig", ":corp:\n");
        let ifdef = "= T\nifdef::corp[]\n:pdf-theme: a\nendif::[]\n";
        let ifndef = "= T\nifndef::corp[]\n:pdf-theme: a\nendif::[]\n";
        let unset = "= T\n:corp!:\nifdef::corp[]\n:pdf-theme: a\nendif::[]\n";
        assert_eq!(theme(&[config, ("main.adoc", ifdef)]), "set:a");
        assert_eq!(theme(&[config, ("main.adoc", ifndef)]), "none");
        assert_eq!(theme(&[config, ("main.adoc", unset)]), "none");
    }

    #[test]
    fn ifeval_is_an_unknown_branch() {
        let text = "= T\nJane\nv1.0\nifeval::[1 == 1]\n:pdf-theme: a\nendif::[]\n";
        assert_eq!(main(text), "uncertain");
    }

    /// The reader consumes a directive line it rejects with an error
    /// (Asciidoctor 2.0.26: `malformed preprocessor directive`) and the
    /// header goes on, as if the line were not there.
    #[test]
    fn a_dropped_directive_is_no_line_of_the_header() {
        assert_eq!(main("= T\nifdef::[]\nJane\nv1.0\n:pdf-theme: a\n"), "set:a");
        assert_eq!(
            main("= T\nJane\nendif::x[text]\nv1.0\n:pdf-theme: a\n"),
            "set:a"
        );
    }

    /// An `endif` for another target than the open block closes nothing
    /// (Asciidoctor 2.0.26: `mismatched preprocessor directive`): the entry
    /// stays in the unknown branch.
    #[test]
    fn an_endif_for_another_target_leaves_the_branch_open() {
        let text = "= T\nifdef::foo[]\nendif::x[]\n:pdf-theme: a\nendif::[]\n";
        assert_eq!(main(text), "uncertain");
        let text = "= T\nifdef::foo[]\nendif::FOO[]\n:pdf-theme: a\n";
        assert_eq!(main(text), "set:a");
    }

    #[test]
    fn a_line_that_may_end_the_header_makes_what_follows_uncertain() {
        let text = "= T\n:pdf-theme: a\nifdef::foo[]\n\nendif::[]\n:x: y\n";
        assert_eq!(main(text), "set:a");
        let text = "= T\n:pdf-theme: a\nifdef::foo[]\n\nendif::[]\n:pdf-theme: b\n";
        assert_eq!(main(text), "uncertain");
        // The same in a branch that holds: the header is over.
        let text = "= T\n:pdf-theme: a\nifdef::backend-pdf[]\n\nendif::[]\n:pdf-theme: b\n";
        assert_eq!(main(text), "set:a");
    }

    #[test]
    fn an_include_that_cannot_be_read_leaves_everything_uncertain() {
        let text = "= T\n:pdf-theme: a\ninclude::missing.adoc[]\n";
        assert_eq!(main(text), "uncertain");
    }

    #[test]
    fn an_include_in_an_unknown_branch_is_read_as_uncertain() {
        let files = [
            (
                "main.adoc",
                "= T\n:pdf-theme: a\nifdef::foo[]\ninclude::inc.adoc[]\nendif::[]\n",
            ),
            ("inc.adoc", ":pdf-theme: b\n"),
        ];
        assert_eq!(theme(&files), "uncertain");
    }

    /// Where a line of an unknown branch ends the header in every reading in
    /// which the branch holds, the rest of the branch can only add
    /// uncertainty: its includes are not read (a missing file would blind the
    /// walk), its entries stay uncertain, and the reading after its `endif`
    /// goes on as before.
    #[test]
    fn an_unknown_branch_that_ended_the_header_is_not_read_on() {
        assert_eq!(
            main("= T\n:pdf-theme: a\nifdef::foo[]\n\ninclude::missing.adoc[]\nendif::[]\n"),
            "set:a"
        );
        assert_eq!(
            main(
                "= T\n:pdf-theme: a\nifdef::foo[]\n\ninclude::missing.adoc[]\n:pdf-theme: b\nendif::[]\n"
            ),
            "uncertain"
        );
        assert_eq!(
            main(
                "= T\n:pdf-theme: a\nifdef::foo[]\n\nifdef::bar[]\nendif::[]\ninclude::missing.adoc[]\nendif::[]\n"
            ),
            "set:a"
        );
        assert_eq!(
            main("= T\n:pdf-theme: a\nifdef::foo[]\n\nendif::[]\ninclude::missing.adoc[]\n"),
            "uncertain"
        );
    }

    /// `RevisionInfoLineRx` takes `:a,`: nothing is needed after the comma
    /// (oracle: Asciidoctor 2.0.26, `doc.attr('pdf-theme')` is `nosuch`).
    #[test]
    fn a_line_that_ends_in_a_comma_is_a_revision_line() {
        for line in [":a,", ":a,:b,", ":a,b", ":a, :b"] {
            let text = format!("= T\nJane\n{line}\n:pdf-theme: nosuch\n");
            assert_eq!(main(&text), "set:nosuch", "{line}");
        }
        assert_eq!(main("= T\nJane\n:a,:b\n:pdf-theme: nosuch\n"), "none");
    }

    #[test]
    fn entries_substitute_what_the_header_defined_before() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let text =
            "= T\n:dir: themes\n:pdf-themesdir: {docdir}/{dir}\n:Undef!:\n:pdf-theme: {undef}\n";
        std::fs::write(root.join("main.adoc"), text).unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        let header = PdfHeader::read(&ws, &root.join("main.adoc"));
        let value = |name| match header.value(name) {
            Some(HeaderValue::Set(entry)) => entry.value.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(value("pdf-themesdir"), format!("{}/themes", root.display()));
        assert_eq!(value("pdf-theme"), "{undef}");
    }
}
