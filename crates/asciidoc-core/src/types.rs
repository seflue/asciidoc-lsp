use std::fmt;
use std::path::PathBuf;

/// Where a title or an include stands in the block tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Among sections: a title there is a section.
    Body,
    /// Inside a delimited block, table cell or list item: Asciidoctor reads
    /// no section there, and an include there reads block content.
    Block,
    /// Inside a part the grammar could not parse (`ERROR`): unknown.
    Unparsed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub level: usize,
    pub title: String,
    pub line: usize,
    /// The ID the section carries itself: `[[id]]`, `[#id]` or `[id=...]`
    /// above the title, else `== Title [[id]]`. With it Asciidoctor generates
    /// no `_title` ID.
    pub id: Option<String>,
    /// The level-0 title is the document header: only blank lines, comments,
    /// attribute entries and an ID line precede it. Asciidoctor generates no
    /// ID for it. A level-0 title elsewhere is a book part and gets one.
    pub is_header: bool,
    /// Block style of the attribute lines above the title (`appendix`,
    /// `discrete`); the last line that sets one wins.
    pub style: Option<String>,
    pub placement: Placement,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IncludeMacro {
    pub path: String,
    pub attributes: Vec<(String, String)>,
    pub line: usize,
    /// Byte column, on `line`, where `path` starts.
    pub target_col: usize,
    pub placement: Placement,
}

/// A source range that starts on a diagnostic's or xref's `line` and may end on a later one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Byte column on the start line.
    pub start_col: usize,
    /// 1-based line the range ends on.
    pub end_line: usize,
    /// Byte column on `end_line`.
    pub end_col: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Xref {
    pub target: String,
    pub display_text: Option<String>,
    pub line: usize,
    /// Range of the xref, from `line` to where it ends (a table cell can wrap it).
    pub span: Option<Span>,
    /// Byte column, on `line`, where `target` starts.
    pub target_col: usize,
}

impl Xref {
    /// The ID the target names with its byte columns on `line`: the part
    /// after `#`, or a plain ID. `None` for a file-only target or an empty
    /// anchor.
    #[must_use]
    pub fn id_span(&self) -> Option<(&str, usize, usize)> {
        let (id, col) = if let Some((file, anchor)) = self.target.split_once('#') {
            (anchor, self.target_col + file.len() + 1)
        } else if self.target.contains('.') {
            return None;
        } else {
            (self.target.as_str(), self.target_col)
        };
        (!id.is_empty()).then(|| (id, col, col + id.len()))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AttributeDef {
    pub name: String,
    pub value: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Anchor {
    pub id: String,
    pub reftext: Option<String>,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AttributeRef {
    pub name: String,
    pub line: usize,      // 1-based
    pub start_col: usize, // byte-offset of opening {
    pub end_col: usize,   // byte-offset of closing }
}

/// A delimited block whose closing delimiter the grammar found missing, see
/// `extract_unterminated_blocks`.
#[derive(Debug, Clone, PartialEq)]
pub struct UnterminatedBlock {
    /// Asciidoctor's name for the block kind: `listing`, `example`, ...
    pub context: &'static str,
    /// The delimiter that closes the block, e.g. `----` (the leading run of
    /// the opener, without trailing blanks or a fence language).
    pub delimiter: String,
    /// 1-based line of the opening delimiter.
    pub line: usize,
}

#[derive(Debug)]
pub struct IncludeTree {
    pub file: PathBuf,
    pub includes: Vec<IncludeTree>,
    pub sections: Vec<Section>,
    pub xrefs: Vec<Xref>,
    pub anchors: Vec<Anchor>,
    pub error: Option<String>,
}

// --- Semantic Layer types ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorKind {
    Explicit,
    ImplicitSection,
    BlockId,
}

#[derive(Debug, Clone)]
pub struct AnchorLocation {
    pub file: PathBuf,
    pub line: usize,
    pub kind: AnchorKind,
}

#[derive(Debug, Clone)]
pub struct IncludeEdge {
    pub target: PathBuf,
    pub line: usize,
    pub attributes: Vec<(String, String)>,
    pub placement: Placement,
}

#[derive(Debug, PartialEq)]
pub struct DocumentModel {
    pub path: PathBuf,
    pub sections: Vec<Section>,
    pub includes: Vec<IncludeMacro>,
    pub xrefs: Vec<Xref>,
    pub anchors: Vec<Anchor>,
    pub attributes: Vec<AttributeDef>,
    pub attribute_refs: Vec<AttributeRef>,
    pub conditionals: Vec<ConditionalDirective>,
    /// Attribute references Asciidoctor substitutes, in document order.
    pub substituted_refs: Vec<AttributeRef>,
    /// Entries the LSP cannot be sure of, see [`PossibleDefinition`].
    pub possible_definitions: Vec<PossibleDefinition>,
    /// Names of the `tag::name[]` directives in the raw lines, see
    /// `extract_tag_directives`.
    pub tags: Vec<String>,
    /// Delimited blocks without a closing delimiter.
    pub unterminated_blocks: Vec<UnterminatedBlock>,
}

/// An attribute that may be defined at `line` without the LSP being able to
/// tell: an entry in a single-line conditional (`ifndef::x[:x: v]`) or a
/// `{counter:x}` / `{set:x:v}` reference. Never reported as undefined from
/// there on, until a sure entry or unset follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PossibleDefinition {
    pub name: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticKind {
    /// `documents` names the top-level documents where the target is
    /// missing when several read the file; empty when the file belongs to
    /// one document only.
    BrokenXref {
        target: String,
        documents: Vec<PathBuf>,
    },
    BrokenInclude {
        path: String,
    },
    /// Tags named by an include's `tag`/`tags` that its file does not open
    /// (`tag::name[]`). `path` is the include path as written.
    IncludeTagNotFound {
        tags: Vec<String>,
        path: String,
    },
    /// `documents` names the top-level documents where the attribute is not
    /// set when several read the file; empty when the file belongs to one
    /// document only.
    UndefinedAttribute {
        name: String,
        documents: Vec<PathBuf>,
    },
    /// `documents` are the top-level documents in which the second
    /// definition (the diagnostic's own position) follows the first.
    DuplicateAnchor {
        id: String,
        /// Display path, see `display_path` in diagnostics.rs: root-relative
        /// under the root or up to two `..` steps away, absolute otherwise;
        /// not an addressable location.
        first_file: PathBuf,
        first_line: usize,
        documents: Vec<PathBuf>,
    },
    CircularInclude {
        path: String,
        /// Display path, see `display_path` in diagnostics.rs: root-relative
        /// under the root or up to two `..` steps away, absolute otherwise;
        /// not an addressable location.
        cycle_through: PathBuf,
    },
    /// A section title deeper than Asciidoctor expects below the section or
    /// document header above it; levels include `leveloffset`.
    SectionOutOfSequence {
        /// The levels accepted there, ascending: `[n]`, or `[0, 1]` at the
        /// top of a book.
        expected: Vec<usize>,
        got: usize,
        /// As in `BrokenXref`: the top-level documents where it is out of
        /// sequence when several read the file; empty otherwise.
        documents: Vec<PathBuf>,
    },
    /// A level 0 section title other than the document header in a document
    /// whose doctype is not `book`; the level includes `leveloffset`.
    LevelZeroSectionOutsideBook {
        /// As in `BrokenXref`: the top-level documents where it is reported
        /// when several read the file; empty otherwise.
        documents: Vec<PathBuf>,
    },
    /// `pdf-themesdir` / `pdf-fontsdir` directory or `pdf-theme` file
    /// that does not exist.
    PdfPathNotFound {
        attribute: String,
        /// Display path (`pdf_display_path` in diagnostics.rs): root-relative
        /// under the root, absolute otherwise; not an addressable location.
        path: PathBuf,
    },
    /// `pdf-theme` ending in `.yaml`: Asciidoctor PDF only loads `.yml`.
    PdfThemeYamlExtension {
        /// Display path (`pdf_display_path` in diagnostics.rs): root-relative
        /// under the root, absolute otherwise; not an addressable location.
        value: String,
    },
    /// `pdf-theme` name without `pdf-themesdir` that no bundled theme has.
    UnknownPdfTheme {
        name: String,
    },
    /// Relative PDF path: resolves against the build's working directory.
    UnanchoredPdfPath {
        attribute: String,
        value: String,
    },
    /// Reserved for future typo detection (Levenshtein against known attributes).
    /// Currently unused: ifdef/ifndef attributes are build-time-dependent,
    /// so the LSP cannot verify them.
    #[allow(dead_code)]
    UndefinedConditionalAttr {
        name: String,
        directive: String,
    },
    /// A delimited block that is never closed; `context` is Asciidoctor's
    /// name for the block kind, `delimiter` the delimiter that closes the block,
    /// e.g. `----`.
    UnterminatedBlock {
        context: String,
        delimiter: String,
    },
}

impl fmt::Display for DiagnosticKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiagnosticKind::BrokenXref { target, documents } => {
                write!(f, "Broken cross-reference: {target}")?;
                if documents.is_empty() {
                    return Ok(());
                }
                let documents: Vec<_> = documents.iter().map(|d| d.display().to_string()).collect();
                write!(f, " (not found in {})", documents.join(", "))
            }
            DiagnosticKind::BrokenInclude { path } => {
                write!(f, "Broken include: {path}")
            }
            DiagnosticKind::IncludeTagNotFound { tags, path } => {
                let noun = if tags.len() == 1 { "tag" } else { "tags" };
                write!(
                    f,
                    "{noun} '{}' not found in include file: {path}",
                    tags.join(", ")
                )
            }
            DiagnosticKind::UndefinedAttribute { name, documents } => {
                write!(f, "Undefined attribute: {{{name}}}")?;
                if documents.is_empty() {
                    return Ok(());
                }
                let documents: Vec<_> = documents.iter().map(|d| d.display().to_string()).collect();
                write!(f, " (not set in {})", documents.join(", "))
            }
            DiagnosticKind::DuplicateAnchor {
                id,
                first_file,
                first_line,
                documents,
            } => {
                let documents: Vec<_> = documents.iter().map(|d| d.display().to_string()).collect();
                write!(
                    f,
                    "Duplicate anchor '{}' in {}, first defined in {}:{}",
                    id,
                    documents.join(", "),
                    first_file.display(),
                    first_line
                )
            }
            DiagnosticKind::CircularInclude {
                path,
                cycle_through,
            } => {
                write!(
                    f,
                    "Circular include: {} creates a cycle through {}",
                    path,
                    cycle_through.display()
                )
            }
            DiagnosticKind::SectionOutOfSequence {
                expected,
                got,
                documents,
            } => {
                let levels: Vec<_> = expected.iter().map(ToString::to_string).collect();
                let plural = if expected.len() > 1 { "s" } else { "" };
                write!(
                    f,
                    "Section title out of sequence: expected level{plural} {}, got level {got}",
                    levels.join(" or ")
                )?;
                if documents.is_empty() {
                    return Ok(());
                }
                let documents: Vec<_> = documents.iter().map(|d| d.display().to_string()).collect();
                write!(f, " (in {})", documents.join(", "))
            }
            DiagnosticKind::LevelZeroSectionOutsideBook { documents } => {
                write!(f, "Level 0 sections can only be used when doctype is book")?;
                if documents.is_empty() {
                    return Ok(());
                }
                let documents: Vec<_> = documents.iter().map(|d| d.display().to_string()).collect();
                write!(f, " (in {})", documents.join(", "))
            }
            DiagnosticKind::PdfPathNotFound { attribute, path } => {
                write!(f, "{attribute}: not found: {}", path.display())
            }
            DiagnosticKind::PdfThemeYamlExtension { value } => {
                write!(
                    f,
                    "pdf-theme: theme file must end in .yml, not .yaml: {value}"
                )
            }
            DiagnosticKind::UnknownPdfTheme { name } => {
                write!(
                    f,
                    "pdf-theme: '{name}' is not a built-in theme; set pdf-themesdir for a custom theme"
                )
            }
            DiagnosticKind::UnanchoredPdfPath { attribute, value } => {
                write!(
                    f,
                    "{attribute}: relative path '{value}' resolves against the build's working directory; anchor it with {{docdir}}"
                )
            }
            DiagnosticKind::UndefinedConditionalAttr { name, directive } => {
                write!(f, "{directive}::{name} references undefined attribute")
            }
            DiagnosticKind::UnterminatedBlock { context, delimiter } => {
                write!(f, "Unterminated {context} block: no closing {delimiter}")
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub file: PathBuf,
    pub line: usize,
    /// Range starting on `line`; `None` covers the whole line.
    pub span: Option<Span>,
    pub kind: DiagnosticKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionalKind {
    Ifdef,
    Ifndef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionalOperator {
    Single,
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConditionalDirective {
    pub kind: ConditionalKind,
    pub attributes: Vec<String>,
    pub operator: ConditionalOperator,
    pub line: usize,
}

/// A location within a file, used for reference results.
///
/// Replaces `(PathBuf, usize, usize, usize)` tuples throughout the codebase.
#[derive(Debug, Clone, PartialEq)]
pub struct FileLocation {
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
    pub end_col: usize,
}

impl FileLocation {
    #[must_use]
    pub fn new(path: PathBuf, line: usize, col: usize, end_col: usize) -> Self {
        Self {
            path,
            line,
            col,
            end_col,
        }
    }
}

#[derive(Debug)]
pub struct ResolvedXref {
    pub file: PathBuf,
    pub line: usize,
    /// What the ID is defined as; `None` for a target that names only a file.
    pub kind: Option<AnchorKind>,
    /// The ID the target resolved to, also when a natural xref names it by
    /// its reftext or title; `None` for a target that names only a file.
    pub id: Option<String>,
}

#[cfg(test)]
impl DocumentModel {
    /// Creates a DocumentModel with the given path and all collections empty.
    pub fn test(path: &str) -> Self {
        Self {
            path: PathBuf::from(path),
            sections: vec![],
            includes: vec![],
            xrefs: vec![],
            anchors: vec![],
            attributes: vec![],
            attribute_refs: vec![],
            conditionals: vec![],
            substituted_refs: vec![],
            possible_definitions: vec![],
            tags: vec![],
            unterminated_blocks: vec![],
        }
    }

    pub fn with_sections(mut self, sections: Vec<Section>) -> Self {
        self.sections = sections;
        self
    }
    pub fn with_includes(mut self, includes: Vec<IncludeMacro>) -> Self {
        self.includes = includes;
        self
    }
    pub fn with_xrefs(mut self, xrefs: Vec<Xref>) -> Self {
        self.xrefs = xrefs;
        self
    }
    pub fn with_anchors(mut self, anchors: Vec<Anchor>) -> Self {
        self.anchors = anchors;
        self
    }
    pub fn with_attributes(mut self, attributes: Vec<AttributeDef>) -> Self {
        self.attributes = attributes;
        self
    }
    pub fn with_attribute_refs(mut self, attribute_refs: Vec<AttributeRef>) -> Self {
        self.attribute_refs = attribute_refs;
        self
    }
    pub fn with_conditionals(mut self, conditionals: Vec<ConditionalDirective>) -> Self {
        self.conditionals = conditionals;
        self
    }
    pub fn with_substituted_refs(mut self, substituted_refs: Vec<AttributeRef>) -> Self {
        self.substituted_refs = substituted_refs;
        self
    }
    pub fn with_unterminated_blocks(mut self, unterminated_blocks: Vec<UnterminatedBlock>) -> Self {
        self.unterminated_blocks = unterminated_blocks;
        self
    }
    pub fn with_possible_definitions(
        mut self,
        possible_definitions: Vec<PossibleDefinition>,
    ) -> Self {
        self.possible_definitions = possible_definitions;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_anchor_kind_variants() {
        let kinds = vec![
            AnchorKind::Explicit,
            AnchorKind::ImplicitSection,
            AnchorKind::BlockId,
        ];
        for kind in &kinds {
            // Debug + Clone work
            let _debug = format!("{:?}", kind);
            let _clone = kind.clone();
        }
        assert_ne!(AnchorKind::Explicit, AnchorKind::ImplicitSection);
    }

    #[test]
    fn test_diagnostic_display() {
        let d1 = DiagnosticKind::BrokenXref {
            target: "missing-id".into(),
            documents: vec![],
        };
        assert_eq!(d1.to_string(), "Broken cross-reference: missing-id");

        let d2 = DiagnosticKind::BrokenInclude {
            path: "missing.adoc".into(),
        };
        assert_eq!(d2.to_string(), "Broken include: missing.adoc");

        let unterminated = DiagnosticKind::UnterminatedBlock {
            context: "listing".into(),
            delimiter: "----".into(),
        };
        assert_eq!(
            unterminated.to_string(),
            "Unterminated listing block: no closing ----"
        );

        let tag = DiagnosticKind::IncludeTagNotFound {
            tags: vec!["a".into()],
            path: "x.yml".into(),
        };
        assert_eq!(tag.to_string(), "tag 'a' not found in include file: x.yml");

        let tags = DiagnosticKind::IncludeTagNotFound {
            tags: vec!["a".into(), "b".into()],
            path: "x.yml".into(),
        };
        assert_eq!(
            tags.to_string(),
            "tags 'a, b' not found in include file: x.yml"
        );

        let d3 = DiagnosticKind::UndefinedAttribute {
            name: "foo".into(),
            documents: vec![],
        };
        assert_eq!(d3.to_string(), "Undefined attribute: {foo}");

        let d3b = DiagnosticKind::UndefinedAttribute {
            name: "foo".into(),
            documents: vec![PathBuf::from("web.adoc"), PathBuf::from("print.adoc")],
        };
        assert_eq!(
            d3b.to_string(),
            "Undefined attribute: {foo} (not set in web.adoc, print.adoc)"
        );

        let d4 = DiagnosticKind::DuplicateAnchor {
            id: "my-id".into(),
            first_file: PathBuf::from("/a/b.adoc"),
            first_line: 5,
            documents: vec![PathBuf::from("/book.adoc"), PathBuf::from("/web.adoc")],
        };
        assert!(d4
            .to_string()
            .contains("Duplicate anchor 'my-id' in /book.adoc, /web.adoc"));
        assert!(d4.to_string().contains("/a/b.adoc:5"));

        let d5 = DiagnosticKind::SectionOutOfSequence {
            expected: vec![3],
            got: 4,
            documents: vec![],
        };
        assert_eq!(
            d5.to_string(),
            "Section title out of sequence: expected level 3, got level 4"
        );

        let d5b = DiagnosticKind::SectionOutOfSequence {
            expected: vec![0, 1],
            got: 2,
            documents: vec![],
        };
        assert_eq!(
            d5b.to_string(),
            "Section title out of sequence: expected levels 0 or 1, got level 2"
        );

        let d5c = DiagnosticKind::SectionOutOfSequence {
            expected: vec![2],
            got: 3,
            documents: vec![PathBuf::from("print.adoc"), PathBuf::from("book.adoc")],
        };
        assert_eq!(
            d5c.to_string(),
            "Section title out of sequence: expected level 2, got level 3 (in print.adoc, book.adoc)"
        );

        let d6 = DiagnosticKind::LevelZeroSectionOutsideBook { documents: vec![] };
        assert_eq!(
            d6.to_string(),
            "Level 0 sections can only be used when doctype is book"
        );

        let d6b = DiagnosticKind::LevelZeroSectionOutsideBook {
            documents: vec![PathBuf::from("web.adoc"), PathBuf::from("print.adoc")],
        };
        assert_eq!(
            d6b.to_string(),
            "Level 0 sections can only be used when doctype is book (in web.adoc, print.adoc)"
        );
    }

    #[test]
    fn test_circular_include_display() {
        let d = DiagnosticKind::CircularInclude {
            path: "b.adoc".into(),
            cycle_through: PathBuf::from("/project/a.adoc"),
        };
        let s = d.to_string();
        assert!(s.contains("Circular include"), "got: {s}");
        assert!(s.contains("b.adoc"), "got: {s}");
        assert!(s.contains("a.adoc"), "got: {s}");
    }

    #[test]
    fn test_conditional_directive_struct() {
        let ifdef = ConditionalDirective {
            kind: ConditionalKind::Ifdef,
            attributes: vec!["backend".to_string()],
            operator: ConditionalOperator::Single,
            line: 5,
        };
        assert_eq!(ifdef.attributes, vec!["backend"]);
        assert_eq!(ifdef.kind, ConditionalKind::Ifdef);
        assert_eq!(ifdef.operator, ConditionalOperator::Single);
        assert_eq!(ifdef.line, 5);

        let ifndef = ConditionalDirective {
            kind: ConditionalKind::Ifndef,
            attributes: vec!["source-dir".to_string()],
            operator: ConditionalOperator::Single,
            line: 10,
        };
        assert_eq!(ifndef.kind, ConditionalKind::Ifndef);

        // Debug + Clone work
        let _debug = format!("{:?}", ifdef);
        let _clone = ifdef.clone();
        assert_ne!(ConditionalKind::Ifdef, ConditionalKind::Ifndef);
        assert_ne!(ConditionalOperator::And, ConditionalOperator::Or);
    }

    #[test]
    fn test_attribute_ref_struct() {
        let attr_ref = AttributeRef {
            name: "imagesdir".to_string(),
            line: 5,
            start_col: 10,
            end_col: 21,
        };
        assert_eq!(attr_ref.name, "imagesdir");
        assert_eq!(attr_ref.line, 5);
        assert_eq!(attr_ref.start_col, 10);
        assert_eq!(attr_ref.end_col, 21);

        // Debug + Clone work
        let _debug = format!("{:?}", attr_ref);
        let _clone = attr_ref.clone();
    }
}
