use std::path::{Path, PathBuf};

use tower_lsp_server::ls_types::Position;

use asciidoc_core::workspace::Workspace;

use crate::positions;

/// Classification of a node found at a cursor position.
#[derive(Debug, PartialEq)]
pub enum NodeKind {
    /// `include::path[]` block macro
    Include { path: String },
    /// `xref:target#anchor[text]` inline macro
    Xref { target: String },
    /// `<<id,text>>` shorthand cross-reference
    ShorthandXref { target: String },
    /// `{name}` attribute reference
    AttributeRef { name: String },
}

/// Information about a recognized node at a cursor position.
#[derive(Debug)]
pub struct NodeInfo {
    pub kind: NodeKind,
    pub line: usize, // 1-indexed
}

/// Find and classify the semantic node at the given LSP position.
///
/// Tries block grammar first (for includes), then inline grammar (for xrefs).
pub fn find_node_at_position(content: &str, pos: Position) -> Option<NodeInfo> {
    let (line_1, byte_col) = positions::from_lsp_position(pos, content);
    let point = tree_sitter::Point {
        row: line_1.saturating_sub(1),
        column: byte_col,
    };

    // Check {attribute} references first — most specific pattern (cursor inside braces).
    // This must come before tree-sitter so that e.g. include::{attr}/file.adoc[]
    // resolves to the attribute when cursor is on {attr}.
    if let Some(info) = find_attribute_ref_at_position(content, line_1, byte_col) {
        return Some(info);
    }

    // Try block grammar (includes)
    let mut block_parser = asciidoc_core::parser::init_block_parser().ok()?;
    let block_tree = asciidoc_core::parser::parse(&mut block_parser, content).ok()?;

    if let Some(info) = classify_block_node(block_tree.root_node(), point, content.as_bytes()) {
        return Some(info);
    }

    // Try inline grammar (xrefs, shorthand xrefs).
    // Only parse the range(s) containing the cursor position to avoid cross-range
    // misparses (e.g. a `+` in one paragraph matching a `+` in another, creating
    // a giant false passthrough node that swallows xref nodes — adoc-0023).
    let inline_ranges = asciidoc_core::extractors::collect_inline_ranges(block_tree.root_node());
    let cursor_ranges: Vec<_> = inline_ranges
        .iter()
        .filter(|r| r.start_point.row <= point.row && r.end_point.row >= point.row)
        .cloned()
        .collect();
    if !cursor_ranges.is_empty() {
        let mut inline_parser = asciidoc_core::parser::init_inline_parser().ok()?;
        inline_parser.set_included_ranges(&cursor_ranges).ok()?;
        let inline_tree = asciidoc_core::parser::parse(&mut inline_parser, content).ok()?;

        if let Some(info) = classify_inline_node(inline_tree.root_node(), point, content.as_bytes())
        {
            return Some(info);
        }
    }

    None
}

/// Resolve a go-to-definition request to a target file and line.
///
/// Returns `(target_path, target_line_1indexed)` or `None` if the node
/// at the cursor is not a navigable reference or cannot be resolved.
///
/// Extracted from the LanguageServer handler for testability (V-8).
pub fn resolve_goto_definition(
    content: &str,
    pos: Position,
    doc_path: &Path,
    workspace: Option<&Workspace>,
) -> Option<(PathBuf, usize)> {
    let node_info = find_node_at_position(content, pos)?;

    match node_info.kind {
        NodeKind::Include { ref path } => {
            let doc_dir = doc_path.parent()?;
            let resolved_path = if let Some(ws) = workspace {
                ws.attributes.substitute(doc_path, path)
            } else {
                path.clone()
            };
            let target = doc_dir.join(&resolved_path);
            let target_canon = target.canonicalize().ok()?;
            Some((target_canon, 1))
        }
        NodeKind::Xref { ref target } | NodeKind::ShorthandXref { ref target } => {
            let ws = workspace?;
            let xref = asciidoc_core::types::Xref {
                target: target.clone(),
                display_text: None,
                line: node_info.line,
            };
            let resolved = ws
                .reference_index
                .resolve_xref(&xref, doc_path, &ws.include_graph, &ws.documents)
                .ok()?;
            Some((resolved.file, resolved.line))
        }
        NodeKind::AttributeRef { ref name } => {
            let ws = workspace?;
            let source = ws.attributes.resolve_source(doc_path, name)?;
            Some((source.file, source.line))
        }
    }
}

// ---------------------------------------------------------------------------
// Internal classification helpers
// ---------------------------------------------------------------------------

/// Check if the node at the given point is inside a `block_macro` with name "include".
fn classify_block_node(
    root: tree_sitter::Node,
    point: tree_sitter::Point,
    source: &[u8],
) -> Option<NodeInfo> {
    let node = root.descendant_for_point_range(point, point)?;

    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == "block_macro" {
            if let Some(macro_name) = find_child_text(n, "block_macro_name", source) {
                if macro_name == "include" {
                    let path = find_child_text(n, "target", source).unwrap_or_default();
                    return Some(NodeInfo {
                        kind: NodeKind::Include { path },
                        line: n.start_position().row + 1,
                    });
                }
            }
        }
        current = n.parent();
    }

    None
}

/// Check if the node at the given point is inside an xref (`inline_macro` or `<<>>`).
fn classify_inline_node(
    root: tree_sitter::Node,
    point: tree_sitter::Point,
    source: &[u8],
) -> Option<NodeInfo> {
    let node = root.descendant_for_point_range(point, point)?;

    let mut current = Some(node);
    while let Some(n) = current {
        match n.kind() {
            "inline_macro" => {
                let macro_name = find_child_text(n, "macro_name", source).unwrap_or_default();
                if macro_name == "xref" {
                    let target = find_child_text(n, "target", source).unwrap_or_default();
                    return Some(NodeInfo {
                        kind: NodeKind::Xref { target },
                        line: n.start_position().row + 1,
                    });
                }
            }
            "xref" => {
                let text = n.utf8_text(source).unwrap_or_default();
                if text.starts_with("<<") {
                    let inner = text.trim_start_matches("<<").trim_end_matches(">>");
                    let target = match inner.split_once(',') {
                        Some((t, _)) => t.trim().to_string(),
                        None => inner.trim().to_string(),
                    };
                    return Some(NodeInfo {
                        kind: NodeKind::ShorthandXref { target },
                        line: n.start_position().row + 1,
                    });
                }
            }
            _ => {}
        }
        current = n.parent();
    }

    None
}

/// Detect `{attribute-name}` references on the current line using regex-like scanning.
/// Returns NodeInfo if the cursor is inside a valid `{name}` pattern
/// (no spaces allowed in attribute names).
fn find_attribute_ref_at_position(
    content: &str,
    line_1: usize,
    byte_col: usize,
) -> Option<NodeInfo> {
    let line_0 = line_1.saturating_sub(1);
    let line_str = content.lines().nth(line_0)?;

    let bytes = line_str.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let start = i + 1;
            let mut end = start;
            let mut valid = true;
            while end < bytes.len() && bytes[end] != b'}' {
                if bytes[end] == b' ' || bytes[end] == b'\t' {
                    valid = false;
                    break;
                }
                end += 1;
            }
            if valid && end < bytes.len() && bytes[end] == b'}' && end > start {
                // {name} found from byte i to end (inclusive)
                if byte_col >= i && byte_col <= end {
                    let name = &line_str[start..end];
                    return Some(NodeInfo {
                        kind: NodeKind::AttributeRef {
                            name: name.to_string(),
                        },
                        line: line_1,
                    });
                }
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }

    None
}

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // --- Task 4.1: find_node_at_position tests ---

    #[test]
    fn test_find_node_include() {
        let content = "= Title\n\ninclude::chapters/intro.adoc[]\n\nSome text.\n";
        // "include::..." is on line index 2 (0-based)
        let pos = Position {
            line: 2,
            character: 12,
        };

        let info = find_node_at_position(content, pos).unwrap();
        assert_eq!(
            info.kind,
            NodeKind::Include {
                path: "chapters/intro.adoc".into()
            }
        );
        assert_eq!(info.line, 3); // 1-indexed
    }

    #[test]
    fn test_find_node_include_at_start() {
        let content = "= Title\n\ninclude::sub.adoc[]\n";
        let pos = Position {
            line: 2,
            character: 0,
        };

        let info = find_node_at_position(content, pos).unwrap();
        assert_eq!(
            info.kind,
            NodeKind::Include {
                path: "sub.adoc".into()
            }
        );
    }

    #[test]
    fn test_find_node_include_on_path() {
        let content = "= Title\n\ninclude::sub.adoc[]\n";
        // character 12 = 's' in "sub.adoc"
        let pos = Position {
            line: 2,
            character: 12,
        };

        let info = find_node_at_position(content, pos).unwrap();
        assert_eq!(
            info.kind,
            NodeKind::Include {
                path: "sub.adoc".into()
            }
        );
    }

    #[test]
    fn test_find_node_xref() {
        let content = "= Title\n\nSee xref:file.adoc#anchor[Link Text] for details.\n";
        // character 12 = 'l' in "file.adoc"
        let pos = Position {
            line: 2,
            character: 12,
        };

        let info = find_node_at_position(content, pos).unwrap();
        assert_eq!(
            info.kind,
            NodeKind::Xref {
                target: "file.adoc#anchor".into()
            }
        );
    }

    #[test]
    fn test_find_node_shorthand_xref() {
        let content = "= Title\n\nSee <<my-section,Section Title>> for details.\n";
        // character 8 = '-' in "my-section"
        let pos = Position {
            line: 2,
            character: 8,
        };

        let info = find_node_at_position(content, pos).unwrap();
        assert_eq!(
            info.kind,
            NodeKind::ShorthandXref {
                target: "my-section".into()
            }
        );
    }

    #[test]
    fn test_find_node_plain_text_returns_none() {
        let content = "= Title\n\nJust plain text.\n";
        let pos = Position {
            line: 2,
            character: 5,
        };

        assert!(find_node_at_position(content, pos).is_none());
    }

    #[test]
    fn test_find_node_section_heading_returns_none() {
        let content = "= Title\n\n== Chapter\n";
        let pos = Position {
            line: 2,
            character: 5,
        };

        assert!(find_node_at_position(content, pos).is_none());
    }

    // --- Task 4.2: resolve_goto_definition for include ---

    #[test]
    fn test_goto_def_include() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("sub.adoc");

        fs::write(&a, "= Doc A\n\ninclude::sub.adoc[]\n").unwrap();
        fs::write(&b, "== Sub Chapter\n").unwrap();

        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 12,
        };

        let (target, line) = resolve_goto_definition(&content, pos, &a, None).unwrap();
        assert_eq!(target, b.canonicalize().unwrap());
        assert_eq!(line, 1);
    }

    #[test]
    fn test_goto_def_include_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Doc A\n\ninclude::nonexistent.adoc[]\n").unwrap();

        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 12,
        };

        assert!(resolve_goto_definition(&content, pos, &a, None).is_none());
    }

    // --- Task 4.3: resolve_goto_definition for xref ---

    #[test]
    fn test_goto_def_xref_file_anchor() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(
            &a,
            "= Doc A\n\nSee xref:b.adoc#my-anchor[link].\n\ninclude::b.adoc[]\n",
        )
        .unwrap();
        fs::write(&b, "[[my-anchor]]\n== Chapter B\n\nContent.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // character 12 = inside "b.adoc#my-anchor"
        let pos = Position {
            line: 2,
            character: 12,
        };

        let (target, line) = resolve_goto_definition(&content, pos, &a, Some(&ws)).unwrap();
        assert_eq!(target, b.canonicalize().unwrap());
        assert_eq!(line, 1); // [[my-anchor]] is on line 1
    }

    #[test]
    fn test_goto_def_xref_shorthand() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(&a, "= Doc A\n\nSee <<my-anchor>>.\n\ninclude::b.adoc[]\n").unwrap();
        fs::write(&b, "[[my-anchor]]\n== Chapter B\n\nContent.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // character 8 = inside "my-anchor"
        let pos = Position {
            line: 2,
            character: 8,
        };

        let (target, line) = resolve_goto_definition(&content, pos, &a, Some(&ws)).unwrap();
        assert_eq!(target, b.canonicalize().unwrap());
        assert_eq!(line, 1);
    }

    #[test]
    fn test_goto_def_xref_broken() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Doc A\n\nSee <<nonexistent-anchor>>.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 8,
        };

        assert!(resolve_goto_definition(&content, pos, &a, Some(&ws)).is_none());
    }

    // --- Task 3.1: resolve_goto_definition for attribute references ---

    #[test]
    fn test_goto_def_attribute_doc_header() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Doc A\n:author: Alice\n\nWritten by {author}.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // character 12 = 'a' in "{author}" on line 3 (0-indexed)
        let pos = Position {
            line: 3,
            character: 12,
        };

        let (target, line) = resolve_goto_definition(&content, pos, &a, Some(&ws)).unwrap();
        assert_eq!(target, a.canonicalize().unwrap());
        assert_eq!(line, 2); // :author: is on line 2 (1-indexed)
    }

    #[test]
    fn test_goto_def_attribute_config() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(".asciidoctorconfig"),
            ":imagesdir: images\n",
        )
        .unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Doc A\n\nImages in {imagesdir} folder.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // character 11 = 'i' in "{imagesdir}" on line 2 (0-indexed)
        let pos = Position {
            line: 2,
            character: 11,
        };

        let (target, line) = resolve_goto_definition(&content, pos, &a, Some(&ws)).unwrap();
        let config_path = tmp
            .path()
            .join(".asciidoctorconfig")
            .canonicalize()
            .unwrap();
        assert_eq!(target, config_path);
        assert_eq!(line, 1);
    }

    #[test]
    fn test_goto_def_attribute_undefined() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Doc A\n\nSee {nonexistent} here.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 5,
        };

        assert!(resolve_goto_definition(&content, pos, &a, Some(&ws)).is_none());
    }

    // --- Task 2.1: find_node_at_position for {attribute} references ---

    #[test]
    fn test_find_node_attribute_ref() {
        let content = "= Title\n\nThe images are in {imagesdir} directory.\n";
        // character 27 = 'i' in "imagesdir" (inside the braces)
        let pos = Position {
            line: 2,
            character: 27,
        };

        let info = find_node_at_position(content, pos).unwrap();
        assert_eq!(
            info.kind,
            NodeKind::AttributeRef {
                name: "imagesdir".into()
            }
        );
    }

    #[test]
    fn test_find_node_attribute_ref_in_include() {
        let content = "= Title\n\ninclude::{source-dir}/file.adoc[]\n";
        // character 10 = 's' in "source-dir" (inside the braces in include target)
        let pos = Position {
            line: 2,
            character: 10,
        };

        let info = find_node_at_position(content, pos).unwrap();
        assert_eq!(
            info.kind,
            NodeKind::AttributeRef {
                name: "source-dir".into()
            }
        );
    }

    #[test]
    fn test_find_node_plain_braces_not_attr() {
        // Empty braces and braces with spaces should not be detected
        let content = "= Title\n\nSome {} text and {with spaces} here.\n";
        // On the empty braces
        let pos1 = Position {
            line: 2,
            character: 5,
        };
        assert!(find_node_at_position(content, pos1).is_none());

        // On "with spaces" — contains space, not a valid attribute name
        let pos2 = Position {
            line: 2,
            character: 27,
        };
        assert!(find_node_at_position(content, pos2).is_none());
    }

    // --- adoc-0023: xref after list items ---

    #[test]
    fn test_find_node_xref_after_list_items() {
        // Reproduces adoc-0023: xref in paragraph after unordered list is not found.
        // This matches the pattern in 09_architecture_decisions.adoc:49.
        let content = "\
= Title

*Consequences:*

* Positive: First benefit.
* Negative: A downside.

For details see xref:05_building_block_view.adoc#building-blocks[see Building Block View].
";
        // "xref:" starts at character 16 on line 7 (0-indexed)
        let pos = Position {
            line: 7,
            character: 20,
        };

        let info = find_node_at_position(content, pos);
        assert!(
            info.is_some(),
            "xref after list items must be found (adoc-0023)"
        );
        let info = info.unwrap();
        assert_eq!(
            info.kind,
            NodeKind::Xref {
                target: "05_building_block_view.adoc#building-blocks".into()
            }
        );
    }

    #[test]
    fn test_find_node_xref_real_adr_content() {
        // adoc-0023: xref in 09_architecture_decisions.adoc line 49 was not found
        // because `+` in "(block + inline)" created a false passthrough spanning
        // rows 17-70, swallowing the xref nodes.
        let content = fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/chapters/09_architecture_decisions.adoc"
        ))
        .unwrap();

        // Line 49 (1-indexed): "...see xref:05_building_block_view.adoc#building-blocks[...]."
        let pos = Position {
            line: 48,
            character: 67,
        };
        let info = find_node_at_position(&content, pos);
        assert!(info.is_some(), "xref on line 49 must be found (adoc-0023)");
        assert_eq!(
            info.unwrap().kind,
            NodeKind::Xref {
                target: "05_building_block_view.adoc#building-blocks".into()
            }
        );
    }

    #[test]
    fn test_find_node_xref_real_adr_line_71() {
        // adoc-0023: second affected xref on line 71, also inside the false passthrough.
        let content = fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/chapters/09_architecture_decisions.adoc"
        ))
        .unwrap();

        // Line 71 (1-indexed): "...documented in xref:05_building_block_view.adoc#building-blocks[...]..."
        // "xref:" starts at character 91, target at 96
        let pos = Position {
            line: 70,
            character: 100,
        };
        let info = find_node_at_position(&content, pos);
        assert!(info.is_some(), "xref on line 71 must be found (adoc-0023)");
        assert_eq!(
            info.unwrap().kind,
            NodeKind::Xref {
                target: "05_building_block_view.adoc#building-blocks".into()
            }
        );
    }
}
