use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub level: usize,
    pub title: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IncludeMacro {
    pub path: String,
    pub attributes: Vec<(String, String)>,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Xref {
    pub target: String,
    pub display_text: Option<String>,
    pub line: usize,
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

// --- Semantic Layer types (adoc-0010) ---

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
        first_file: PathBuf,
    },
    CircularInclude {
        path: String,
        cycle_through: PathBuf,
    },
    UndefinedConditionalAttr {
        name: String,
        directive: String,
    },
}

impl fmt::Display for DiagnosticKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiagnosticKind::BrokenXref { target } => {
                write!(f, "broken cross-reference: {target}")
            }
            DiagnosticKind::BrokenInclude { path } => {
                write!(f, "broken include: {path}")
            }
            DiagnosticKind::UndefinedAttribute { name } => {
                write!(f, "undefined attribute: {{{name}}}")
            }
            DiagnosticKind::DuplicateAnchor { id, first_file } => {
                write!(
                    f,
                    "duplicate anchor '{}', first defined in {}",
                    id,
                    first_file.display()
                )
            }
            DiagnosticKind::CircularInclude {
                path,
                cycle_through,
            } => {
                write!(
                    f,
                    "circular include: {} creates cycle through {}",
                    path,
                    cycle_through.display()
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
    pub kind: DiagnosticKind,
    pub message: String,
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
    fn test_circular_include_display() {
        let d = DiagnosticKind::CircularInclude {
            path: "b.adoc".into(),
            cycle_through: PathBuf::from("/project/a.adoc"),
        };
        let s = d.to_string();
        assert!(s.contains("circular include"), "got: {s}");
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
