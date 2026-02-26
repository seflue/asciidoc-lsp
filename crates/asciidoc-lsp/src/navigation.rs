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

    // Try block grammar first (includes)
    let mut block_parser = asciidoc_core::parser::init_block_parser().ok()?;
    let block_tree = asciidoc_core::parser::parse(&mut block_parser, content).ok()?;

    if let Some(info) = classify_block_node(block_tree.root_node(), point, content.as_bytes()) {
        return Some(info);
    }

    // Try inline grammar (xrefs, shorthand xrefs)
    let inline_ranges = asciidoc_core::extractors::collect_inline_ranges(block_tree.root_node());
    if inline_ranges.is_empty() {
        return None;
    }

    let mut inline_parser = asciidoc_core::parser::init_inline_parser().ok()?;
    inline_parser.set_included_ranges(&inline_ranges).ok()?;
    let inline_tree = asciidoc_core::parser::parse(&mut inline_parser, content).ok()?;

    classify_inline_node(inline_tree.root_node(), point, content.as_bytes())
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
}
