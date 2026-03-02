use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Section {
    pub level: usize,
    pub title: String,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct IncludeMacro {
    pub path: String,
    pub attributes: Vec<(String, String)>,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct Xref {
    pub target: String,
    pub display_text: Option<String>,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct AttributeDef {
    pub name: String,
    pub value: String,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct Anchor {
    pub id: String,
    pub line: usize,
}

#[derive(Debug, Clone)]
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

// --- Semantic Layer types (adoc-0010) ---
// allow(dead_code): These types are used incrementally as phases are implemented.
// The allow will be removed once all modules reference them.

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum AnchorKind {
    Explicit,
    ImplicitSection,
    BlockId,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AnchorLocation {
    pub file: PathBuf,
    pub line: usize,
    pub kind: AnchorKind,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct IncludeEdge {
    pub target: PathBuf,
    pub line: usize,
    pub attributes: Vec<(String, String)>,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct DocumentModel {
    pub path: PathBuf,
    pub sections: Vec<Section>,
    pub includes: Vec<IncludeMacro>,
    pub xrefs: Vec<Xref>,
    pub anchors: Vec<Anchor>,
    pub attributes: Vec<AttributeDef>,
    pub attribute_refs: Vec<AttributeRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum DiagnosticKind {
    BrokenXref { target: String },
    BrokenInclude { path: String },
    UndefinedAttribute { name: String },
    DuplicateAnchor { id: String, first_file: PathBuf },
}

impl fmt::Display for DiagnosticKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiagnosticKind::BrokenXref { target } => {
                write!(f, "broken cross-reference: {}", target)
            }
            DiagnosticKind::BrokenInclude { path } => {
                write!(f, "broken include: {}", path)
            }
            DiagnosticKind::UndefinedAttribute { name } => {
                write!(f, "undefined attribute: {{{}}}", name)
            }
            DiagnosticKind::DuplicateAnchor { id, first_file } => {
                write!(
                    f,
                    "duplicate anchor '{}', first defined in {}",
                    id,
                    first_file.display()
                )
            }
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Diagnostic {
    pub file: PathBuf,
    pub line: usize,
    pub kind: DiagnosticKind,
    pub message: String,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct ResolvedXref {
    pub file: PathBuf,
    pub line: usize,
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
        assert_eq!(d1.to_string(), "broken cross-reference: missing-id");

        let d2 = DiagnosticKind::BrokenInclude {
            path: "missing.adoc".into(),
        };
        assert_eq!(d2.to_string(), "broken include: missing.adoc");

        let d3 = DiagnosticKind::UndefinedAttribute { name: "foo".into() };
        assert_eq!(d3.to_string(), "undefined attribute: {foo}");

        let d4 = DiagnosticKind::DuplicateAnchor {
            id: "my-id".into(),
            first_file: PathBuf::from("/a/b.adoc"),
        };
        assert!(d4.to_string().contains("duplicate anchor 'my-id'"));
        assert!(d4.to_string().contains("/a/b.adoc"));
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
