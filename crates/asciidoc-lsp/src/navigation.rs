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
    /// `:name: value` attribute definition
    AttributeDef { name: String },
    /// `[[id]]` anchor / ID assignment
    Anchor { id: String },
    /// `image::path[]` block macro
    Image { path: String },
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

    // Check :name: value attribute definitions — cursor anywhere on the line.
    if let Some(info) = find_attribute_def_at_position(content, line_1) {
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
        .copied()
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
/// Extracted from the `LanguageServer` handler for testability (V-8).
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
        NodeKind::Image { ref path } => {
            let doc_dir = doc_path.parent()?;
            let resolved_path = if let Some(ws) = workspace {
                ws.attributes.substitute(doc_path, path)
            } else {
                path.clone()
            };
            let imagesdir_raw = workspace
                .and_then(|ws| ws.attributes.resolve(doc_path, "imagesdir"))
                .unwrap_or("");
            let imagesdir = if let Some(ws) = workspace {
                ws.attributes.substitute(doc_path, imagesdir_raw)
            } else {
                imagesdir_raw.to_string()
            };
            let target = if imagesdir.is_empty() {
                doc_dir.join(&resolved_path)
            } else {
                doc_dir.join(imagesdir).join(&resolved_path)
            };
            let target_canon = target.canonicalize().ok()?;
            Some((target_canon, 1))
        }
        NodeKind::AttributeDef { .. } | NodeKind::Anchor { .. } => {
            // Already on the definition — goto-definition is a no-op
            None
        }
    }
}

/// Resolve a find-references request to a list of reference locations.
///
/// Returns `(file_path, line_1indexed, start_col, end_col)` for each reference.
/// When `include_declaration` is true and cursor is on a `{name}` reference,
/// the definition location is included in the results.
pub fn resolve_references(
    content: &str,
    pos: Position,
    doc_path: &Path,
    workspace: Option<&Workspace>,
    include_declaration: bool,
) -> Option<Vec<(PathBuf, usize, usize, usize)>> {
    let node_info = find_node_at_position(content, pos)?;
    let ws = workspace?;

    let mut results = match &node_info.kind {
        NodeKind::AttributeRef { name } | NodeKind::AttributeDef { name } => {
            let mut r = ws.find_all_attribute_usages(name);
            if include_declaration && matches!(node_info.kind, NodeKind::AttributeRef { .. }) {
                if let Some(source) = ws.attributes.resolve_source(doc_path, name) {
                    r.push((source.file, source.line, 0, 0));
                }
            }
            r
        }
        NodeKind::Include { path } => {
            let doc_dir = doc_path.parent()?;
            let resolved_path = ws.attributes.substitute(doc_path, path);
            let target = doc_dir.join(&resolved_path);
            let target_canonical = target.canonicalize().ok()?;
            let filename = target_canonical
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("");

            let mut r = ws.find_all_include_references(&target_canonical);
            r.extend(ws.find_all_file_xrefs(filename));
            r
        }
        NodeKind::Anchor { id } => {
            let mut r = ws.find_all_anchor_references(id);
            if include_declaration {
                if let Some(loc) = ws.reference_index.lookup(id) {
                    r.push((loc.file.clone(), loc.line, 0, 0));
                }
            }
            r
        }
        NodeKind::Xref { target } | NodeKind::ShorthandXref { target } => {
            // Extract anchor part from xref target
            let anchor_id = if let Some((_file, anchor)) = target.split_once('#') {
                if anchor.is_empty() {
                    return None;
                }
                anchor
            } else if target.contains('.') {
                return None; // file-only xref, no anchor to find references for
            } else {
                target.as_str()
            };
            let mut r = ws.find_all_anchor_references(anchor_id);
            if include_declaration {
                if let Some(loc) = ws.reference_index.lookup(anchor_id) {
                    r.push((loc.file.clone(), loc.line, 0, 0));
                }
            }
            r
        }
        NodeKind::Image { .. } => {
            // Images have no references semantics — find-references is a no-op
            return None;
        }
    };

    // Deduplicate results (same file+line can appear from multiple sources)
    results.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    results.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);

    if results.is_empty() {
        None
    } else {
        Some(results)
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
                if macro_name == "image" {
                    let path = find_child_text(n, "target", source).unwrap_or_default();
                    return Some(NodeInfo {
                        kind: NodeKind::Image { path },
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
            "id_assignment" => {
                // [[anchor-id]] — the `id` child node contains the anchor text
                if let Some(id) = find_child_text(n, "id", source) {
                    return Some(NodeInfo {
                        kind: NodeKind::Anchor { id },
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

/// Detect `:name: value` attribute definitions on the current line.
/// Returns `NodeInfo` if the line matches the `:name: value` pattern.
fn find_attribute_def_at_position(content: &str, line_1: usize) -> Option<NodeInfo> {
    let line_0 = line_1.saturating_sub(1);
    let line_str = content.lines().nth(line_0)?;
    let bytes = line_str.as_bytes();

    // Pattern: ^:([a-zA-Z0-9_-]+):\s+ or ^:([a-zA-Z0-9_-]+):$
    if bytes.first() != Some(&b':') {
        return None;
    }

    let mut end = 1;
    while end < bytes.len() && bytes[end] != b':' {
        let b = bytes[end];
        if !(b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return None;
        }
        end += 1;
    }

    // Must have closing `:` and name must be non-empty
    if end >= bytes.len() || end <= 1 {
        return None;
    }

    // After closing `:` must be end-of-line or whitespace
    let after_colon = end + 1;
    if after_colon < bytes.len() && bytes[after_colon] != b' ' && bytes[after_colon] != b'\t' {
        return None;
    }

    let name = &line_str[1..end];
    Some(NodeInfo {
        kind: NodeKind::AttributeDef {
            name: name.to_string(),
        },
        line: line_1,
    })
}

/// Detect `{attribute-name}` references on the current line using regex-like scanning.
/// Returns `NodeInfo` if the cursor is inside a valid `{name}` pattern
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

    // --- Task 2.2: find_node_at_position for attribute definitions ---

    #[test]
    fn test_find_node_attribute_def() {
        let content = "= Title\n:imagesdir: images\n\nContent.\n";
        // Cursor on "imagesdir" in the definition line (line 1, 0-indexed)
        let pos = Position {
            line: 1,
            character: 5,
        };

        let info = find_node_at_position(content, pos).unwrap();
        assert_eq!(
            info.kind,
            NodeKind::AttributeDef {
                name: "imagesdir".into()
            }
        );
    }

    // --- adoc-0025: resolve_references for attribute references ---

    #[test]
    fn test_references_from_attribute_ref() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(".asciidoctorconfig"),
            ":imagesdir: images\n",
        )
        .unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");
        fs::write(
            &a,
            "= Doc A\n\nImages in {imagesdir} folder.\n\ninclude::b.adoc[]\n",
        )
        .unwrap();
        fs::write(&b, "== Chapter B\n\nAlso {imagesdir} here.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // cursor on {imagesdir} in a.adoc — "Images in {imagesdir}" line 2 (0-indexed)
        // '{' is at byte 10, 'i' at 11
        let pos = Position {
            line: 2,
            character: 11,
        };

        let results = resolve_references(&content, pos, &a, Some(&ws), false).unwrap();
        assert_eq!(results.len(), 2, "Expected 2 usages, got: {:?}", results);
    }

    #[test]
    fn test_references_from_attribute_def() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Doc A\n:myattr: value\n\nUsing {myattr} here.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // cursor on :myattr: definition, line 1 (0-indexed), character 3
        let pos = Position {
            line: 1,
            character: 3,
        };

        let results = resolve_references(&content, pos, &a, Some(&ws), false).unwrap();
        assert_eq!(results.len(), 1, "Expected 1 usage, got: {:?}", results);
    }

    #[test]
    fn test_references_include_declaration() {
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
        // cursor on {imagesdir}
        let pos = Position {
            line: 2,
            character: 11,
        };

        // Without includeDeclaration — just usages
        let without = resolve_references(&content, pos, &a, Some(&ws), false).unwrap();
        assert_eq!(without.len(), 1);

        // With includeDeclaration — usages + definition
        let with = resolve_references(&content, pos, &a, Some(&ws), true).unwrap();
        assert_eq!(
            with.len(),
            2,
            "Expected usage + declaration, got: {:?}",
            with
        );

        // The declaration should point to .asciidoctorconfig
        let config_path = tmp
            .path()
            .join(".asciidoctorconfig")
            .canonicalize()
            .unwrap();
        let has_config = with.iter().any(|(p, _, _, _)| *p == config_path);
        assert!(
            has_config,
            "Declaration from .asciidoctorconfig should be included"
        );
    }

    // --- adoc-0025 Task 4.1: Integration test — gr on {attr} in .adoc ---

    #[test]
    fn test_integration_references_attribute_usage() {
        // Multi-file workspace: config defines attribute, 3 .adoc files use it.
        // references() on {source-dir} in chapter should find all usages across workspace.
        let tmp = tempfile::tempdir().unwrap();
        let chapters = tmp.path().join("chapters");
        fs::create_dir(&chapters).unwrap();

        fs::write(
            tmp.path().join(".asciidoctorconfig"),
            ":source-dir: ../src\n:imagesdir: images\n",
        )
        .unwrap();

        let index = tmp.path().join("index.adoc");
        fs::write(
            &index,
            "= Architecture\n\ninclude::chapters/concepts.adoc[]\n\ninclude::chapters/building.adoc[]\n",
        )
        .unwrap();

        fs::write(
            chapters.join("concepts.adoc"),
            "== Concepts\n\nSource code is in {source-dir} directory.\n",
        )
        .unwrap();

        fs::write(
            chapters.join("building.adoc"),
            "== Building Blocks\n\nSee {source-dir}/main.rs for entry point.\n\nImages in {imagesdir}.\n",
        )
        .unwrap();

        let ws = Workspace::build(tmp.path(), &index).unwrap();

        // Cursor on {source-dir} in concepts.adoc, line 2 (0-indexed)
        let concepts_content = fs::read_to_string(chapters.join("concepts.adoc")).unwrap();
        let concepts_path = chapters.join("concepts.adoc").canonicalize().unwrap();
        let pos = Position {
            line: 2,
            character: 19, // inside {source-dir}
        };

        let results =
            resolve_references(&concepts_content, pos, &concepts_path, Some(&ws), false).unwrap();
        // {source-dir} is used in concepts.adoc (line 3) and building.adoc (line 3)
        assert_eq!(
            results.len(),
            2,
            "Expected 2 usages of {{source-dir}}, got: {:?}",
            results
        );

        // Verify both files are represented
        let building_path = chapters.join("building.adoc").canonicalize().unwrap();
        let files: Vec<_> = results.iter().map(|(p, _, _, _)| p.clone()).collect();
        assert!(
            files.contains(&concepts_path),
            "concepts.adoc should be in results"
        );
        assert!(
            files.contains(&building_path),
            "building.adoc should be in results"
        );
    }

    // --- adoc-0025 Task 4.2: Integration test — gr on definition in .asciidoctorconfig ---

    #[test]
    fn test_integration_references_from_config() {
        // Cursor on :source-dir: ../src in .asciidoctorconfig.
        // references() should find all {source-dir} usages in .adoc files.
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join(".asciidoctorconfig");
        fs::write(&config_path, ":source-dir: ../src\n:imagesdir: images\n").unwrap();

        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");
        fs::write(
            &a,
            "= Doc A\n\nCode in {source-dir} folder.\n\ninclude::b.adoc[]\n",
        )
        .unwrap();
        fs::write(&b, "== Chapter B\n\nAlso see {source-dir}/lib.rs.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();

        // Read .asciidoctorconfig content and place cursor on :source-dir:
        let config_content = fs::read_to_string(&config_path).unwrap();
        let pos = Position {
            line: 0,
            character: 5, // inside "source-dir" in ":source-dir: ../src"
        };

        let results =
            resolve_references(&config_content, pos, &config_path, Some(&ws), false).unwrap();
        assert_eq!(
            results.len(),
            2,
            "Expected 2 usages of {{source-dir}} from config, got: {:?}",
            results
        );
    }

    // --- adoc-0026 Task 1.1: find_node_at_position for anchors ---

    #[test]
    fn test_find_node_anchor() {
        let content = "= Title\n\n[[my-anchor]]\n== Section\n\nContent.\n";
        // Cursor on "my-anchor" inside [[my-anchor]], line 2 (0-indexed)
        let pos = Position {
            line: 2,
            character: 4,
        };

        let info = find_node_at_position(content, pos).unwrap();
        assert_eq!(
            info.kind,
            NodeKind::Anchor {
                id: "my-anchor".into()
            }
        );
        assert_eq!(info.line, 3); // 1-indexed
    }

    #[test]
    fn test_find_node_anchor_in_context() {
        // Anchor before a section heading
        let content = "= Title\n\n[[intro]]\n== Introduction\n\nSome text.\n";
        let pos = Position {
            line: 2,
            character: 3,
        };

        let info = find_node_at_position(content, pos).unwrap();
        assert_eq!(info.kind, NodeKind::Anchor { id: "intro".into() });
    }

    // --- adoc-0026 Task 2.1: resolve_references for includes ---

    #[test]
    fn test_references_from_include() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");
        let c = tmp.path().join("c.adoc");

        fs::write(&a, "= Doc A\n\ninclude::b.adoc[]\n\ninclude::c.adoc[]\n").unwrap();
        fs::write(&b, "== Chapter B\n\nContent.\n").unwrap();
        fs::write(&c, "== Chapter C\n\ninclude::b.adoc[]\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // cursor on include::b.adoc[]
        let pos = Position {
            line: 2,
            character: 12,
        };

        let results = resolve_references(&content, pos, &a, Some(&ws), false).unwrap();
        // b.adoc is included from a.adoc (line 3) and c.adoc (line 3)
        assert_eq!(results.len(), 2, "Expected 2 refs, got: {:?}", results);
    }

    #[test]
    fn test_references_from_include_with_xrefs() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(
            &a,
            "= Doc A\n\ninclude::b.adoc[]\n\nSee xref:b.adoc#anchor[link].\n",
        )
        .unwrap();
        fs::write(&b, "[[anchor]]\n== Chapter B\n\nContent.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // cursor on include::b.adoc[]
        let pos = Position {
            line: 2,
            character: 12,
        };

        let results = resolve_references(&content, pos, &a, Some(&ws), false).unwrap();
        // include ref + xref to b.adoc
        assert_eq!(
            results.len(),
            2,
            "Expected 2 refs (include + xref), got: {:?}",
            results
        );
    }

    // --- adoc-0026 Task 2.2: resolve_references for anchors ---

    #[test]
    fn test_references_from_anchor() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(
            &a,
            "= Doc A\n\n[[my-id]]\n== Section\n\ninclude::b.adoc[]\n",
        )
        .unwrap();
        fs::write(
            &b,
            "== Chapter B\n\nSee <<my-id>>.\n\nAlso xref:a.adoc#my-id[link].\n",
        )
        .unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // cursor on [[my-id]]
        let pos = Position {
            line: 2,
            character: 4,
        };

        let results = resolve_references(&content, pos, &a, Some(&ws), false).unwrap();
        assert_eq!(results.len(), 2, "Expected 2 refs, got: {:?}", results);
    }

    #[test]
    fn test_references_from_anchor_include_declaration() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(
            &a,
            "= Doc A\n\n[[my-id]]\n== Section\n\ninclude::b.adoc[]\n",
        )
        .unwrap();
        fs::write(&b, "== Chapter B\n\nSee <<my-id>>.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // cursor on [[my-id]]
        let pos = Position {
            line: 2,
            character: 4,
        };

        // Without includeDeclaration
        let without = resolve_references(&content, pos, &a, Some(&ws), false).unwrap();
        assert_eq!(without.len(), 1, "Expected 1 ref without declaration");

        // With includeDeclaration — should include anchor definition
        let with = resolve_references(&content, pos, &a, Some(&ws), true).unwrap();
        assert_eq!(
            with.len(),
            2,
            "Expected 2 refs with declaration, got: {:?}",
            with
        );
    }

    // --- adoc-0026 Task 2.3: resolve_references for xrefs ---

    #[test]
    fn test_references_from_xref() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(
            &a,
            "= Doc A\n\n[[target]]\n== Section\n\ninclude::b.adoc[]\n",
        )
        .unwrap();
        fs::write(
            &b,
            "== Chapter B\n\nSee xref:a.adoc#target[link].\n\nAlso <<target>>.\n",
        )
        .unwrap();

        let ws = Workspace::build(tmp.path(), &b).unwrap();
        let content = fs::read_to_string(&b).unwrap();
        // cursor on xref:a.adoc#target[link]
        let pos = Position {
            line: 2,
            character: 8,
        };

        let results = resolve_references(&content, pos, &b, Some(&ws), false).unwrap();
        // Both xref and shorthand xref reference "target"
        assert_eq!(results.len(), 2, "Expected 2 co-refs, got: {:?}", results);
    }

    #[test]
    fn test_references_from_shorthand_xref() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(
            &a,
            "= Doc A\n\n[[anchor]]\n== Section\n\ninclude::b.adoc[]\n",
        )
        .unwrap();
        fs::write(
            &b,
            "== Chapter B\n\nSee <<anchor>>.\n\nAlso xref:a.adoc#anchor[link].\n",
        )
        .unwrap();

        let ws = Workspace::build(tmp.path(), &b).unwrap();
        let content = fs::read_to_string(&b).unwrap();
        // cursor on <<anchor>>
        let pos = Position {
            line: 2,
            character: 7,
        };

        let results = resolve_references(&content, pos, &b, Some(&ws), false).unwrap();
        assert_eq!(results.len(), 2, "Expected 2 co-refs, got: {:?}", results);
    }

    // --- adoc-0026 Task 3.1: Integration test — gr auf Include (Multi-File) ---

    #[test]
    fn test_integration_include_references_multi_file() {
        // arc42-like structure: index includes intro + concepts, concepts xrefs intro
        let tmp = tempfile::tempdir().unwrap();
        let chapters = tmp.path().join("chapters");
        fs::create_dir(&chapters).unwrap();

        let index = tmp.path().join("index.adoc");
        fs::write(
            &index,
            "= Architecture\n\ninclude::chapters/intro.adoc[]\n\ninclude::chapters/concepts.adoc[]\n",
        )
        .unwrap();
        fs::write(
            chapters.join("intro.adoc"),
            "[[intro]]\n== Introduction\n\nWelcome.\n",
        )
        .unwrap();
        fs::write(
            chapters.join("concepts.adoc"),
            "== Concepts\n\nSee xref:intro.adoc#intro[Introduction].\n",
        )
        .unwrap();

        let ws = Workspace::build(tmp.path(), &index).unwrap();
        let content = fs::read_to_string(&index).unwrap();
        // cursor on include::chapters/intro.adoc[]
        let pos = Position {
            line: 2,
            character: 15,
        };

        let results = resolve_references(&content, pos, &index, Some(&ws), false).unwrap();
        // Should find: 1 include ref (from index.adoc) + 1 xref (from concepts.adoc)
        assert_eq!(
            results.len(),
            2,
            "Expected 2 refs (include + xref), got: {:?}",
            results
        );
    }

    // --- adoc-0026 Task 3.2: Integration test — gr auf Anchor (Multi-File) ---

    #[test]
    fn test_integration_anchor_references_multi_file() {
        // a.adoc defines [[my-anchor]], b.adoc and c.adoc reference it
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");
        let c = tmp.path().join("c.adoc");

        fs::write(
            &a,
            "= Doc A\n\n[[my-anchor]]\n== Section\n\ninclude::b.adoc[]\n\ninclude::c.adoc[]\n",
        )
        .unwrap();
        fs::write(&b, "== Chapter B\n\nSee xref:a.adoc#my-anchor[link].\n").unwrap();
        fs::write(&c, "== Chapter C\n\nAlso <<my-anchor>>.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // cursor on [[my-anchor]]
        let pos = Position {
            line: 2,
            character: 5,
        };

        let results = resolve_references(&content, pos, &a, Some(&ws), false).unwrap();
        // Should find 2 refs: xref in b.adoc + shorthand xref in c.adoc
        assert_eq!(
            results.len(),
            2,
            "Expected 2 anchor refs from 2 files, got: {:?}",
            results
        );

        // Verify both files are represented
        let b_canon = b.canonicalize().unwrap();
        let c_canon = c.canonicalize().unwrap();
        let files: Vec<_> = results.iter().map(|(p, _, _, _)| p.clone()).collect();
        assert!(files.contains(&b_canon), "b.adoc should be in results");
        assert!(files.contains(&c_canon), "c.adoc should be in results");
    }

    #[test]
    fn test_find_node_attribute_def_not_on_prefix() {
        let content = "= Title\n:source-dir: ../src\n\nContent.\n";
        // Cursor on the leading `:` (character 0) — should still detect
        let pos = Position {
            line: 1,
            character: 0,
        };

        let info = find_node_at_position(content, pos).unwrap();
        assert_eq!(
            info.kind,
            NodeKind::AttributeDef {
                name: "source-dir".into()
            }
        );
    }

    // --- adoc-0033 Task 1.1: find_node_at_position for image block macro ---

    #[test]
    fn test_find_node_image() {
        let content = "= Title\n\nimage::images/diagram.png[Alt]\n\nSome text.\n";
        // "image::..." is on line index 2 (0-based), cursor inside the macro
        let pos = Position {
            line: 2,
            character: 12,
        };

        let info = find_node_at_position(content, pos).unwrap();
        assert_eq!(
            info.kind,
            NodeKind::Image {
                path: "images/diagram.png".into()
            }
        );
        assert_eq!(info.line, 3); // 1-indexed
    }

    #[test]
    fn test_find_node_image_at_start() {
        let content = "= Title\n\nimage::logo.png[]\n";
        let pos = Position {
            line: 2,
            character: 0,
        };

        let info = find_node_at_position(content, pos).unwrap();
        assert_eq!(
            info.kind,
            NodeKind::Image {
                path: "logo.png".into()
            }
        );
    }

    // --- adoc-0033 Task 2.1: resolve_goto_definition for image ---

    #[test]
    fn test_goto_def_image_simple() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let img = tmp.path().join("diagram.png");

        fs::write(&a, "= Doc A\n\nimage::diagram.png[Alt]\n").unwrap();
        fs::write(&img, "fake-png-data").unwrap();

        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 10,
        };

        let (target, line) = resolve_goto_definition(&content, pos, &a, None).unwrap();
        assert_eq!(target, img.canonicalize().unwrap());
        assert_eq!(line, 1);
    }

    #[test]
    fn test_goto_def_image_with_imagesdir() {
        let tmp = tempfile::tempdir().unwrap();
        let images = tmp.path().join("images");
        fs::create_dir(&images).unwrap();

        fs::write(
            tmp.path().join(".asciidoctorconfig"),
            ":imagesdir: images\n",
        )
        .unwrap();

        let a = tmp.path().join("a.adoc");
        let img = images.join("diagram.png");
        fs::write(&a, "= Doc A\n\nimage::diagram.png[Alt]\n").unwrap();
        fs::write(&img, "fake-png-data").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 10,
        };

        let (target, line) = resolve_goto_definition(&content, pos, &a, Some(&ws)).unwrap();
        assert_eq!(target, img.canonicalize().unwrap());
        assert_eq!(line, 1);
    }

    #[test]
    fn test_goto_def_image_with_attr_in_path() {
        let tmp = tempfile::tempdir().unwrap();
        let mydir = tmp.path().join("assets");
        fs::create_dir(&mydir).unwrap();

        fs::write(tmp.path().join(".asciidoctorconfig"), ":mydir: assets\n").unwrap();

        let a = tmp.path().join("a.adoc");
        let img = mydir.join("logo.png");
        fs::write(&a, "= Doc A\n\nimage::{mydir}/logo.png[Logo]\n").unwrap();
        fs::write(&img, "fake-png-data").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // Cursor after the attribute ref (on "/logo.png" part)
        let pos = Position {
            line: 2,
            character: 20,
        };

        let (target, line) = resolve_goto_definition(&content, pos, &a, Some(&ws)).unwrap();
        assert_eq!(target, img.canonicalize().unwrap());
        assert_eq!(line, 1);
    }

    #[test]
    fn test_goto_def_image_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Doc A\n\nimage::nonexistent.png[Alt]\n").unwrap();

        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 10,
        };

        assert!(resolve_goto_definition(&content, pos, &a, None).is_none());
    }
}
