use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub level: usize,
    pub title: String,
    pub line: usize,
    /// The section carries an ID of its own (`[[id]]`, `[#id]`, `[id=...]`),
    /// so Asciidoctor generates no `_title` ID for it.
    pub explicit_id: bool,
    /// The level-0 title is the document header: only blank lines, comments,
    /// attribute entries and an ID line precede it. Asciidoctor generates no
    /// ID for it. A level-0 title elsewhere is a book part and gets one.
    pub is_header: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IncludeMacro {
    pub path: String,
    pub attributes: Vec<(String, String)>,
    pub line: usize,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticKind {
    BrokenXref {
        target: String,
    },
    BrokenInclude {
        path: String,
    },
    UndefinedAttribute {
        name: String,
    },
    DuplicateAnchor {
        id: String,
        /// Display path, see `display_path` in diagnostics.rs: root-relative
        /// under the root or up to two `..` steps away, absolute otherwise;
        /// not an addressable location.
        first_file: PathBuf,
        first_line: usize,
    },
    CircularInclude {
        path: String,
        /// Display path, see `display_path` in diagnostics.rs: root-relative
        /// under the root or up to two `..` steps away, absolute otherwise;
        /// not an addressable location.
        cycle_through: PathBuf,
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
}

impl fmt::Display for DiagnosticKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiagnosticKind::BrokenXref { target } => {
                write!(f, "Broken cross-reference: {target}")
            }
            DiagnosticKind::BrokenInclude { path } => {
                write!(f, "Broken include: {path}")
            }
            DiagnosticKind::UndefinedAttribute { name } => {
                write!(f, "Undefined attribute: {{{name}}}")
            }
            DiagnosticKind::DuplicateAnchor {
                id,
                first_file,
                first_line,
            } => {
                write!(
                    f,
                    "Duplicate anchor '{}', first defined in {}:{}",
                    id,
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
        };
        assert_eq!(d1.to_string(), "Broken cross-reference: missing-id");

        let d2 = DiagnosticKind::BrokenInclude {
            path: "missing.adoc".into(),
        };
        assert_eq!(d2.to_string(), "Broken include: missing.adoc");

        let d3 = DiagnosticKind::UndefinedAttribute { name: "foo".into() };
        assert_eq!(d3.to_string(), "Undefined attribute: {foo}");

        let d4 = DiagnosticKind::DuplicateAnchor {
            id: "my-id".into(),
            first_file: PathBuf::from("/a/b.adoc"),
            first_line: 5,
        };
        assert!(d4.to_string().contains("Duplicate anchor 'my-id'"));
        assert!(d4.to_string().contains("/a/b.adoc:5"));
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
