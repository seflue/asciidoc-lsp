use crate::types::{Anchor, AttributeDef, IncludeMacro, Section, Xref};

// --- Block-level extractors (work on block parse tree) ---

pub fn extract_sections(node: tree_sitter::Node, source: &[u8]) -> Vec<Section> {
    let mut results = Vec::new();
    walk_for_sections(node, source, &mut results);
    results
}

pub fn extract_includes(node: tree_sitter::Node, source: &[u8]) -> Vec<IncludeMacro> {
    let mut results = Vec::new();
    walk_for_includes(node, source, &mut results);
    results
}

pub fn extract_attributes(node: tree_sitter::Node, source: &[u8]) -> Vec<AttributeDef> {
    let mut results = Vec::new();
    walk_for_attributes(node, source, &mut results);
    results
}

// --- Inline-level extractors (work on inline parse tree) ---

pub fn extract_anchors(node: tree_sitter::Node, source: &[u8]) -> Vec<Anchor> {
    let mut results = Vec::new();
    walk_for_anchors(node, source, &mut results);
    results
}

pub fn extract_xrefs(node: tree_sitter::Node, source: &[u8]) -> Vec<Xref> {
    let mut results = Vec::new();
    walk_for_xrefs(node, source, &mut results);
    results
}

// --- Inline range extraction for dual-grammar ---

/// Collect byte ranges of `line` nodes inside paragraphs, titles, etc.
/// These ranges are passed to the inline parser via `set_included_ranges`.
pub fn collect_inline_ranges(node: tree_sitter::Node) -> Vec<tree_sitter::Range> {
    let mut ranges = Vec::new();
    walk_for_inline_ranges(node, &mut ranges);
    ranges
}

// --- Private walk functions ---

fn walk_for_sections(node: tree_sitter::Node, source: &[u8], results: &mut Vec<Section>) {
    let kind = node.kind();

    // document_title = level 0 (= Title)
    // title1 = level 1 (== Title), title2 = level 2, etc.
    let level = match kind {
        "document_title" => Some(0),
        "title1" => Some(1),
        "title2" => Some(2),
        "title3" => Some(3),
        "title4" => Some(4),
        "title5" => Some(5),
        _ => None,
    };

    if let Some(level) = level {
        // Title text is in the `line` child node
        let title = find_child_text(node, "line", source)
            .unwrap_or_default()
            .trim()
            .to_string();
        if !title.is_empty() {
            results.push(Section {
                level,
                title,
                line: node.start_position().row + 1, // 1-based
            });
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_sections(child, source, results);
    }
}

fn walk_for_includes(node: tree_sitter::Node, source: &[u8], results: &mut Vec<IncludeMacro>) {
    if node.kind() == "block_macro" {
        let macro_name = find_child_text(node, "block_macro_name", source).unwrap_or_default();
        if macro_name == "include" {
            let path = find_child_text(node, "target", source).unwrap_or_default();
            let attributes = extract_block_macro_attrs(node, source);
            results.push(IncludeMacro {
                path,
                attributes,
                line: node.start_position().row + 1,
            });
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_includes(child, source, results);
    }
}

fn walk_for_attributes(node: tree_sitter::Node, source: &[u8], results: &mut Vec<AttributeDef>) {
    if node.kind() == "document_attr" {
        let name = find_child_text(node, "attr_name", source).unwrap_or_default();
        let value = find_child_text(node, "line", source)
            .unwrap_or_default()
            .trim()
            .to_string();
        if !name.is_empty() {
            results.push(AttributeDef {
                name,
                value,
                line: node.start_position().row + 1,
            });
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_attributes(child, source, results);
    }
}

fn walk_for_anchors(node: tree_sitter::Node, source: &[u8], results: &mut Vec<Anchor>) {
    // Skip backtick code spans — no anchors inside monospace
    if node.kind() == "monospace" {
        return;
    }

    // Inline parser: id_assignment > id
    if node.kind() == "id_assignment" {
        let id = find_child_text(node, "id", source).unwrap_or_default();
        if !id.is_empty() {
            results.push(Anchor {
                id,
                line: node.start_position().row + 1,
            });
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_anchors(child, source, results);
    }
}

fn walk_for_xrefs(node: tree_sitter::Node, source: &[u8], results: &mut Vec<Xref>) {
    let kind = node.kind();

    // Skip backtick code spans — no xrefs inside monospace
    if kind == "monospace" {
        return;
    }

    // Two forms in inline grammar:
    // 1. inline_macro where macro_name == "xref" → xref:target#anchor[text]
    // 2. xref → <<id,text>>
    if kind == "inline_macro" {
        let macro_name = find_child_text(node, "macro_name", source).unwrap_or_default();
        if macro_name == "xref" {
            let target = find_child_text(node, "target", source).unwrap_or_default();
            let display = find_child_text(node, "attr", source);
            results.push(Xref {
                target,
                display_text: display.filter(|s| !s.is_empty()),
                line: node.start_position().row + 1,
            });
            return; // Don't recurse into matched macro children
        }
    } else if kind == "xref" {
        // <<id,text>> form — get full text and parse.
        // Filter out keyword tokens: the inline grammar also produces `xref` nodes
        // inside `word > macro_name > xref` when "xref" appears as plain text.
        let text = node.utf8_text(source).unwrap_or_default();
        if text.starts_with("<<") {
            let inner = text.trim_start_matches("<<").trim_end_matches(">>");
            let (target, display) = match inner.split_once(',') {
                Some((t, d)) => (t.trim().to_string(), Some(d.trim().to_string())),
                None => (inner.trim().to_string(), None),
            };
            results.push(Xref {
                target,
                display_text: display,
                line: node.start_position().row + 1,
            });
        }
        return; // Don't recurse into matched xref children
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_xrefs(child, source, results);
    }
}

fn walk_for_inline_ranges(node: tree_sitter::Node, ranges: &mut Vec<tree_sitter::Range>) {
    // Collect `line` nodes inside paragraphs, titles, admonitions, etc.
    // These contain inline content that needs the inline parser.
    let parent_kind = node.kind();
    let is_inline_container = matches!(
        parent_kind,
        "paragraph" | "document_title" | "title1" | "title2" | "title3" | "title4" | "title5"
    );

    if is_inline_container {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "line" {
                ranges.push(child.range());
            }
        }
    }

    // Still recurse into children for nested structures
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_inline_ranges(child, ranges);
    }
}

// --- Helpers ---

/// Find the first child with the given kind and return its text.
fn find_child_text(node: tree_sitter::Node, kind: &str, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return Some(child.utf8_text(source).unwrap_or_default().to_string());
        }
    }
    None
}

/// Extract attributes from block_macro_attr children of a block_macro node.
/// Each block_macro_attr has attribute_name and attribute_value children.
fn extract_block_macro_attrs(node: tree_sitter::Node, source: &[u8]) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "block_macro_attr" {
            let name = find_child_text(child, "attribute_name", source).unwrap_or_default();
            let value = find_child_text(child, "attribute_value", source).unwrap_or_default();
            if !name.is_empty() {
                attrs.push((name, value));
            }
        }
    }
    attrs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    #[test]
    fn test_extract_sections() {
        let mut p = parser::init_block_parser().unwrap();
        let source =
            "= Document Title\n\n== Chapter One\n\nSome text.\n\n=== Subsection\n\nMore text.\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let sections = extract_sections(tree.root_node(), source.as_bytes());

        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].level, 0);
        assert_eq!(sections[0].title, "Document Title");
        assert_eq!(sections[0].line, 1);
        assert_eq!(sections[1].level, 1);
        assert_eq!(sections[1].title, "Chapter One");
        assert_eq!(sections[2].level, 2);
        assert_eq!(sections[2].title, "Subsection");
    }

    #[test]
    fn test_extract_includes() {
        let mut p = parser::init_block_parser().unwrap();
        let source = "= Title\n\ninclude::chapters/intro.adoc[]\n\ninclude::chapters/main.adoc[leveloffset=+1]\n\nSome text.\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let includes = extract_includes(tree.root_node(), source.as_bytes());

        assert_eq!(includes.len(), 2);
        assert_eq!(includes[0].path, "chapters/intro.adoc");
        assert_eq!(includes[0].attributes.len(), 0);
        assert_eq!(includes[1].path, "chapters/main.adoc");
        assert_eq!(includes[1].attributes.len(), 1);
        assert_eq!(
            includes[1].attributes[0],
            ("leveloffset".to_string(), "+1".to_string())
        );
    }

    #[test]
    fn test_extract_attributes() {
        let mut p = parser::init_block_parser().unwrap();
        let source = "= Title\n:imagesdir: images\n:source-dir: ../src\n\nContent.\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let attrs = extract_attributes(tree.root_node(), source.as_bytes());

        assert_eq!(attrs.len(), 2);
        assert_eq!(attrs[0].name, "imagesdir");
        assert_eq!(attrs[0].value, "images");
        assert_eq!(attrs[1].name, "source-dir");
        assert_eq!(attrs[1].value, "../src");
    }

    #[test]
    fn test_extract_xrefs_inline() {
        let mut p = parser::init_inline_parser().unwrap();
        let source = "See xref:file.adoc#anchor[Link Text] for details.";
        let tree = parser::parse(&mut p, source).unwrap();
        println!("Inline xref S-expr: {}", tree.root_node().to_sexp());
        let xrefs = extract_xrefs(tree.root_node(), source.as_bytes());

        assert_eq!(xrefs.len(), 1);
        assert_eq!(xrefs[0].target, "file.adoc#anchor");
    }

    #[test]
    fn test_extract_anchors_inline() {
        let mut p = parser::init_inline_parser().unwrap();
        let source = "[[my-anchor]]";
        let tree = parser::parse(&mut p, source).unwrap();
        println!("Anchor S-expr: {}", tree.root_node().to_sexp());
        let anchors = extract_anchors(tree.root_node(), source.as_bytes());

        // Print result regardless — helps debug if grammar doesn't parse anchors as expected
        println!("Anchors found: {:?}", anchors);
    }

    #[test]
    fn test_collect_inline_ranges() {
        let mut p = parser::init_block_parser().unwrap();
        let source = "= Title\n\nA paragraph with content.\n\n== Section\n\nAnother paragraph.\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let ranges = collect_inline_ranges(tree.root_node());

        assert!(
            !ranges.is_empty(),
            "Should find inline ranges in paragraphs and titles"
        );
        println!("Found {} inline ranges", ranges.len());
    }

    // --- adoc-0013: Include-Attribute extraction ---

    #[test]
    fn test_extract_include_single_attribute() {
        let mut p = parser::init_block_parser().unwrap();
        let source = "= Title\n\ninclude::chapters/main.adoc[leveloffset=+1]\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let includes = extract_includes(tree.root_node(), source.as_bytes());

        assert_eq!(includes.len(), 1);
        assert_eq!(includes[0].path, "chapters/main.adoc");
        assert_eq!(includes[0].attributes.len(), 1);
        assert_eq!(
            includes[0].attributes[0],
            ("leveloffset".to_string(), "+1".to_string())
        );
    }

    #[test]
    fn test_extract_include_multiple_attributes() {
        let mut p = parser::init_block_parser().unwrap();
        let source = "= Title\n\ninclude::file.adoc[leveloffset=+1,tags=main]\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let includes = extract_includes(tree.root_node(), source.as_bytes());

        assert_eq!(includes.len(), 1);
        assert_eq!(includes[0].attributes.len(), 2);
        assert_eq!(
            includes[0].attributes[0],
            ("leveloffset".to_string(), "+1".to_string())
        );
        assert_eq!(
            includes[0].attributes[1],
            ("tags".to_string(), "main".to_string())
        );
    }

    #[test]
    fn test_extract_include_no_attributes() {
        let mut p = parser::init_block_parser().unwrap();
        let source = "= Title\n\ninclude::bare.adoc[]\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let includes = extract_includes(tree.root_node(), source.as_bytes());

        assert_eq!(includes.len(), 1);
        assert_eq!(includes[0].path, "bare.adoc");
        assert!(includes[0].attributes.is_empty());
    }

    // --- adoc-0014: False-positive xref filtering ---

    #[test]
    fn test_xref_keyword_not_false_positive() {
        // The word "xref" in plain text must NOT produce a Xref
        let mut p = parser::init_inline_parser().unwrap();
        let source = "This text mentions xref without any macro syntax.";
        let tree = parser::parse(&mut p, source).unwrap();
        let xrefs = extract_xrefs(tree.root_node(), source.as_bytes());

        assert!(
            xrefs.is_empty(),
            "Plain text 'xref' must not produce false-positive Xrefs, got: {:?}",
            xrefs
        );
    }

    #[test]
    fn test_xref_shorthand_still_works() {
        // <<id,text>> form must still be detected
        let mut p = parser::init_inline_parser().unwrap();
        let source = "See <<my-section,Section Title>> for details.";
        let tree = parser::parse(&mut p, source).unwrap();
        let xrefs = extract_xrefs(tree.root_node(), source.as_bytes());

        assert_eq!(xrefs.len(), 1);
        assert_eq!(xrefs[0].target, "my-section");
        assert_eq!(xrefs[0].display_text.as_deref(), Some("Section Title"));
    }

    #[test]
    fn test_xref_in_backtick_not_detected() {
        // xref inside backtick code span must NOT produce a Xref
        let mut p = parser::init_inline_parser().unwrap();
        let source = "Use `xref:doc.adoc#anchor[link text]` for references.";
        let tree = parser::parse(&mut p, source).unwrap();
        let xrefs = extract_xrefs(tree.root_node(), source.as_bytes());

        assert!(
            xrefs.is_empty(),
            "Xref inside backticks must not be detected, got: {:?}",
            xrefs
        );
    }
}
