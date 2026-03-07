use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tower_lsp_server::ls_types::Position;

use asciidoc_core::types::AnchorKind;
use asciidoc_core::workspace::Workspace;

use crate::positions;

/// Classification of a node found at a cursor position.
#[derive(Debug, PartialEq)]
pub(crate) enum NodeKind {
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
pub(crate) struct NodeInfo {
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

/// Resolve a hover request to a formatted Markdown string.
///
/// Returns hover content for the node at the cursor position:
/// - `AttributeRef`: value + definition location
/// - `AttributeDef`: resolved value
/// - `Xref`/`ShorthandXref`: section title + file
/// - `Include`: resolved path + file info
pub fn resolve_hover(
    content: &str,
    pos: Position,
    doc_path: &Path,
    workspace: Option<&Workspace>,
) -> Option<String> {
    use std::fmt::Write;

    let node_info = find_node_at_position(content, pos)?;

    match &node_info.kind {
        NodeKind::AttributeRef { name } => {
            let ws = workspace?;
            let value = ws.attributes.resolve(doc_path, name)?;
            let mut text = format!(":{name}: {value}");
            if let Some(source) = ws.attributes.resolve_source(doc_path, name) {
                let display_path = file_display_name(&source.file);
                let _ = write!(text, "\n\nDefined in {display_path}:{}", source.line);
            }
            Some(text)
        }
        NodeKind::AttributeDef { name } => {
            let ws = workspace?;
            let value = ws.attributes.resolve(doc_path, name)?;
            Some(format!(":{name}: {value}"))
        }
        NodeKind::Xref { target } | NodeKind::ShorthandXref { target } => {
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

            // Find section title at the resolved location
            let section_title = ws.documents.get(&resolved.file).and_then(|doc| {
                doc.sections
                    .iter()
                    .find(|s| s.line == resolved.line)
                    .map(|s| s.title.clone())
            });

            let display_path = file_display_name(&resolved.file);

            if let Some(title) = section_title {
                Some(format!(
                    "\u{2192} {title}\n\n{display_path}:{}",
                    resolved.line
                ))
            } else {
                // Anchor without section (explicit anchor)
                let anchor_id = target.rsplit('#').next().unwrap_or(target);
                Some(format!(
                    "[[{anchor_id}]]\n\n{display_path}:{}",
                    resolved.line
                ))
            }
        }
        NodeKind::Include { path } => {
            let doc_dir = doc_path.parent()?;
            let resolved_path = if let Some(ws) = workspace {
                ws.attributes.substitute(doc_path, path)
            } else {
                path.clone()
            };
            let target = doc_dir.join(&resolved_path);

            let mut text = format!("include::{resolved_path}[]");
            if target.exists() {
                let section_count = workspace
                    .and_then(|ws| ws.documents.get(&target.canonicalize().ok()?))
                    .map_or(0, |doc| doc.sections.len());
                let _ = write!(text, "\n\n\u{2713} File exists | {section_count} sections");
            } else {
                text.push_str("\n\n\u{2717} File not found");
            }
            Some(text)
        }
        NodeKind::Image { path } => {
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
                doc_dir.join(&imagesdir).join(&resolved_path)
            };

            let display = if imagesdir.is_empty() {
                resolved_path.clone()
            } else {
                format!("{imagesdir}/{resolved_path}")
            };

            let mut text = format!("image::{display}[]");
            if target.exists() {
                text.push_str("\n\n\u{2713} File exists");
            } else {
                text.push_str("\n\n\u{2717} File not found");
            }
            Some(text)
        }
        NodeKind::Anchor { .. } => None,
    }
}

/// Extract a display-friendly filename from a path.
fn file_display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |f| f.to_string_lossy().into_owned(),
    )
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
                if text.starts_with("<<") && text.ends_with(">>") {
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
/// Find the `{name}` attribute reference span containing `byte_col` on `line_str`.
///
/// Returns `(brace_start, name_start, name_end, brace_end)` where:
/// - `brace_start` = index of `{`
/// - `name_start` = index of first char of the name (`brace_start` + 1)
/// - `name_end` = index past last char of the name
/// - `brace_end` = index of `}`
fn find_attr_ref_span(line_str: &str, byte_col: usize) -> Option<(usize, usize, usize, usize)> {
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
                if byte_col >= i && byte_col <= end {
                    return Some((i, start, end, end));
                }
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    None
}

fn find_attribute_ref_at_position(
    content: &str,
    line_1: usize,
    byte_col: usize,
) -> Option<NodeInfo> {
    let line_0 = line_1.saturating_sub(1);
    let line_str = content.lines().nth(line_0)?;

    let (_, name_start, name_end, _) = find_attr_ref_span(line_str, byte_col)?;
    let name = &line_str[name_start..name_end];
    Some(NodeInfo {
        kind: NodeKind::AttributeRef {
            name: name.to_string(),
        },
        line: line_1,
    })
}

/// Find the first child with the given kind and return its text.
fn find_child_text(node: tree_sitter::Node, kind: &str, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return child
                .utf8_text(source)
                .ok()
                .map(std::string::ToString::to_string);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Rename support (adoc-0036)
// ---------------------------------------------------------------------------

/// Map of file path → list of `(line, start_col, end_col)` edits per file.
type RenameEdits = HashMap<PathBuf, Vec<(usize, usize, usize)>>;

/// Result of a prepare-rename check: the symbol's placeholder text and its range on the line.
#[derive(Debug, PartialEq)]
pub(crate) struct PrepareRenameResult {
    pub placeholder: String,
    pub line: usize,      // 1-indexed
    pub start_col: usize, // byte offset
    pub end_col: usize,   // byte offset (exclusive)
}

/// Check whether the cursor is on a rename-able symbol and return its range.
///
/// Returns `None` for non-renameable nodes (includes, images, implicit section anchors).
pub fn prepare_rename(
    content: &str,
    pos: Position,
    _doc_path: &Path,
    workspace: Option<&Workspace>,
) -> Option<PrepareRenameResult> {
    let node_info = find_node_at_position(content, pos)?;
    let line_0 = node_info.line.saturating_sub(1);
    let line_text = content.lines().nth(line_0)?;

    match &node_info.kind {
        NodeKind::Anchor { id } => {
            // Reject implicit section IDs
            if let Some(ws) = workspace {
                if let Some(loc) = ws.reference_index.lookup(id) {
                    if loc.kind == AnchorKind::ImplicitSection {
                        return None;
                    }
                }
            }
            let (start, end) = find_anchor_def_columns(line_text, id)?;
            Some(PrepareRenameResult {
                placeholder: id.clone(),
                line: node_info.line,
                start_col: start,
                end_col: end,
            })
        }
        NodeKind::Xref { target } | NodeKind::ShorthandXref { target } => {
            let anchor_id = if let Some((_file, anchor)) = target.split_once('#') {
                if anchor.is_empty() {
                    return None;
                }
                anchor
            } else if target.contains('.') {
                return None; // file-only xref
            } else {
                target.as_str()
            };
            // Reject implicit section IDs
            if let Some(ws) = workspace {
                if let Some(loc) = ws.reference_index.lookup(anchor_id) {
                    if loc.kind == AnchorKind::ImplicitSection {
                        return None;
                    }
                }
            }
            let (start, end) = find_anchor_ref_columns(line_text, anchor_id)?;
            Some(PrepareRenameResult {
                placeholder: anchor_id.to_string(),
                line: node_info.line,
                start_col: start,
                end_col: end,
            })
        }
        NodeKind::AttributeRef { name } => {
            let (_, byte_col) = positions::from_lsp_position(pos, content);
            let line_str = content.lines().nth(line_0)?;
            let (_, start, end, _) = find_attr_ref_span(line_str, byte_col)?;
            if &line_str[start..end] != name {
                return None;
            }
            Some(PrepareRenameResult {
                placeholder: name.clone(),
                line: node_info.line,
                start_col: start,
                end_col: end,
            })
        }
        NodeKind::AttributeDef { name } => {
            let (start, end) = find_attr_def_columns(line_text, name)?;
            Some(PrepareRenameResult {
                placeholder: name.clone(),
                line: node_info.line,
                start_col: start,
                end_col: end,
            })
        }
        NodeKind::Include { .. } | NodeKind::Image { .. } => None,
    }
}

/// Resolve a rename request: find all locations of the symbol and return edits.
///
/// Returns a map of file path → list of `(line, start_col, end_col)` for each edit.
/// The caller converts this to a `WorkspaceEdit`.
pub fn resolve_rename(
    content: &str,
    pos: Position,
    doc_path: &Path,
    workspace: Option<&Workspace>,
    overrides: &HashMap<PathBuf, String>,
) -> Option<RenameEdits> {
    let node_info = find_node_at_position(content, pos)?;
    let ws = workspace?;

    match &node_info.kind {
        NodeKind::AttributeRef { name } | NodeKind::AttributeDef { name } => {
            let mut edits: HashMap<PathBuf, Vec<(usize, usize, usize)>> = HashMap::new();

            // Attribute references: {name} → start_col+1 to end_col-1 (inside braces)
            let refs = ws.find_all_attribute_usages(name);
            for (path, line, start_col, end_col) in refs {
                edits
                    .entry(path)
                    .or_default()
                    .push((line, start_col + 1, end_col - 1));
            }

            // Attribute definition: :name: → find columns via text search
            if let Some(source) = ws.attributes.resolve_source(doc_path, name) {
                let file_content = read_content(&source.file, overrides);
                let def_line = file_content
                    .lines()
                    .nth(source.line.saturating_sub(1))
                    .unwrap_or_default();
                if let Some((start, end)) = find_attr_def_columns(def_line, name) {
                    edits
                        .entry(source.file)
                        .or_default()
                        .push((source.line, start, end));
                }
            }

            if edits.is_empty() {
                None
            } else {
                Some(edits)
            }
        }
        NodeKind::Anchor { id } => resolve_rename_anchor(id, ws, overrides),
        NodeKind::Xref { target } | NodeKind::ShorthandXref { target } => {
            let anchor_id = if let Some((_file, anchor)) = target.split_once('#') {
                if anchor.is_empty() {
                    return None;
                }
                anchor
            } else if target.contains('.') {
                return None;
            } else {
                target.as_str()
            };
            resolve_rename_anchor(anchor_id, ws, overrides)
        }
        NodeKind::Include { .. } | NodeKind::Image { .. } => None,
    }
}

/// Rename all occurrences of an anchor (definition + all references + duplicates).
fn resolve_rename_anchor(
    anchor_id: &str,
    ws: &Workspace,
    overrides: &HashMap<PathBuf, String>,
) -> Option<RenameEdits> {
    let mut edits: HashMap<PathBuf, Vec<(usize, usize, usize)>> = HashMap::new();
    let mut file_cache: HashMap<PathBuf, String> = HashMap::new();

    // Anchor definition
    if let Some(loc) = ws.reference_index.lookup(anchor_id) {
        let file_content = read_file_cached(&mut file_cache, &loc.file, overrides);
        let line_text = file_content
            .lines()
            .nth(loc.line.saturating_sub(1))
            .unwrap_or_default();
        if let Some((start, end)) = find_anchor_def_columns(line_text, anchor_id) {
            edits
                .entry(loc.file.clone())
                .or_default()
                .push((loc.line, start, end));
        }
    }

    // Duplicate anchor definitions
    for (dup_id, dup_loc) in &ws.reference_index.duplicates {
        if dup_id == anchor_id {
            let file_content = read_file_cached(&mut file_cache, &dup_loc.file, overrides);
            let line_text = file_content
                .lines()
                .nth(dup_loc.line.saturating_sub(1))
                .unwrap_or_default();
            if let Some((start, end)) = find_anchor_def_columns(line_text, anchor_id) {
                edits
                    .entry(dup_loc.file.clone())
                    .or_default()
                    .push((dup_loc.line, start, end));
            }
        }
    }

    // Anchor references (xrefs)
    let refs = ws.find_all_anchor_references(anchor_id);
    for (path, line, _start_col, _end_col) in refs {
        let file_content = read_file_cached(&mut file_cache, &path, overrides);
        let line_text = file_content
            .lines()
            .nth(line.saturating_sub(1))
            .unwrap_or_default();
        if let Some((start, end)) = find_anchor_ref_columns(line_text, anchor_id) {
            edits.entry(path).or_default().push((line, start, end));
        }
    }

    if edits.is_empty() {
        None
    } else {
        Some(edits)
    }
}

/// Read file content, preferring in-memory overrides over disk.
fn read_content(path: &Path, overrides: &HashMap<PathBuf, String>) -> String {
    if let Some(content) = overrides.get(path) {
        return content.clone();
    }
    match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) => {
            tracing::warn!("failed to read file for rename: {}: {e}", path.display());
            String::new()
        }
    }
}

/// Read a file, caching the result. Prefers in-memory overrides over disk.
fn read_file_cached<'a>(
    cache: &'a mut HashMap<PathBuf, String>,
    path: &Path,
    overrides: &HashMap<PathBuf, String>,
) -> &'a str {
    cache
        .entry(path.to_path_buf())
        .or_insert_with(|| read_content(path, overrides))
}

// ---------------------------------------------------------------------------
// Column-finder helpers (adoc-0036, Task 2.1)
// ---------------------------------------------------------------------------

/// Find the byte offsets of `id` inside `[[id]]` on a line.
/// Returns `(start, end)` byte offsets of the id (exclusive end).
fn find_anchor_def_columns(line_text: &str, id: &str) -> Option<(usize, usize)> {
    let pattern = format!("[[{id}]]");
    let pos = line_text.find(&pattern)?;
    let start = pos + 2; // skip `[[`
    Some((start, start + id.len()))
}

/// Find the byte offsets of `id` in an xref reference on a line.
///
/// Searches in priority:
/// 1. `xref:...#id[` (cross-doc) → offset of `id` after `#`
/// 2. `xref:id[` (same-doc) → offset of `id` after `xref:`
/// 3. `<<id` (shorthand) → offset of `id` after `<<`
fn find_anchor_ref_columns(line_text: &str, id: &str) -> Option<(usize, usize)> {
    // 1. xref:...#id[ — search all occurrences to find the right one
    let hash_pattern = format!("#{id}");
    let mut search_from = 0;
    while let Some(rel_pos) = line_text[search_from..].find(&hash_pattern) {
        let hash_pos = search_from + rel_pos;
        // Find the closest preceding xref: for this specific #
        let before = &line_text[..hash_pos];
        if let Some(xref_start) = before.rfind("xref:") {
            // Ensure no '[' between xref: and # (which would mean a different, closed xref)
            let between = &line_text[xref_start + 5..hash_pos];
            if !between.contains('[') {
                let start = hash_pos + 1; // skip #
                return Some((start, start + id.len()));
            }
        }
        search_from = hash_pos + 1;
    }

    // 2. xref:id[
    let xref_pattern = format!("xref:{id}[");
    if let Some(xref_pos) = line_text.find(&xref_pattern) {
        let start = xref_pos + 5; // skip "xref:"
        return Some((start, start + id.len()));
    }

    // 3. <<id (shorthand)
    let shorthand_pattern = format!("<<{id}");
    if let Some(sh_pos) = line_text.find(&shorthand_pattern) {
        let start = sh_pos + 2; // skip "<<"
        return Some((start, start + id.len()));
    }

    None
}

/// Find the byte offsets of `name` inside `:name:` at the start of a line.
/// Returns `(start, end)` byte offsets of the name (exclusive end).
fn find_attr_def_columns(line_text: &str, name: &str) -> Option<(usize, usize)> {
    let pattern = format!(":{name}:");
    if line_text.starts_with(&pattern) {
        Some((1, 1 + name.len()))
    } else {
        None
    }
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

    #[test]
    fn test_goto_def_pagebreak_no_match() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Doc A\n\n<<<\n\nSome content.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // cursor on the page break line
        let pos = Position {
            line: 2,
            character: 1,
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

    // --- adoc-0030: resolve_hover tests ---

    #[test]
    fn test_hover_attribute_ref_with_source() {
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
        let pos = Position {
            line: 2,
            character: 11,
        };

        let hover = resolve_hover(&content, pos, &a, Some(&ws)).unwrap();
        assert!(hover.contains(":imagesdir: images"), "Should show value");
        assert!(
            hover.contains("Defined in .asciidoctorconfig:1"),
            "Should show source, got: {hover}"
        );
    }

    #[test]
    fn test_hover_attribute_ref_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Doc A\n\nSee {nonexistent} here.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 5,
        };

        assert!(resolve_hover(&content, pos, &a, Some(&ws)).is_none());
    }

    #[test]
    fn test_hover_attribute_def() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Doc A\n:myattr: hello world\n\nContent.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 1,
            character: 3,
        };

        let hover = resolve_hover(&content, pos, &a, Some(&ws)).unwrap();
        assert_eq!(hover, ":myattr: hello world");
    }

    #[test]
    fn test_hover_attribute_builtin_no_source() {
        // Builtin attributes like {docdir} have a value but no source location
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Doc A\n:custom: val\n\nSee {custom}.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 3,
            character: 5,
        };

        let hover = resolve_hover(&content, pos, &a, Some(&ws)).unwrap();
        assert!(hover.contains(":custom: val"));
        // Source location should be present since it's defined in the doc header
        assert!(hover.contains("Defined in"));
    }

    #[test]
    fn test_hover_xref_with_section() {
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
        let pos = Position {
            line: 2,
            character: 12,
        };

        let hover = resolve_hover(&content, pos, &a, Some(&ws)).unwrap();
        // Anchor is on line 1 which is [[my-anchor]], not a section line.
        // The section "Chapter B" is on line 2 — but resolved.line points to the anchor line.
        assert!(
            hover.contains("b.adoc:1"),
            "Should show file + line, got: {hover}"
        );
    }

    #[test]
    fn test_hover_shorthand_xref() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(&a, "= Doc A\n\nSee <<my-anchor>>.\n\ninclude::b.adoc[]\n").unwrap();
        fs::write(&b, "[[my-anchor]]\n== Chapter B\n\nContent.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 8,
        };

        let hover = resolve_hover(&content, pos, &a, Some(&ws)).unwrap();
        assert!(
            hover.contains("b.adoc:1"),
            "Should show file + line, got: {hover}"
        );
    }

    #[test]
    fn test_hover_xref_broken() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Doc A\n\nSee <<nonexistent>>.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 8,
        };

        assert!(resolve_hover(&content, pos, &a, Some(&ws)).is_none());
    }

    #[test]
    fn test_hover_include_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("sub.adoc");

        fs::write(&a, "= Doc A\n\ninclude::sub.adoc[]\n").unwrap();
        fs::write(&b, "== Sub Chapter\n\nContent.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 12,
        };

        let hover = resolve_hover(&content, pos, &a, Some(&ws)).unwrap();
        assert!(
            hover.contains("include::sub.adoc[]"),
            "Should show include path, got: {hover}"
        );
        assert!(
            hover.contains("\u{2713} File exists"),
            "Should confirm existence, got: {hover}"
        );
        assert!(
            hover.contains("1 sections"),
            "Should show section count, got: {hover}"
        );
    }

    #[test]
    fn test_hover_include_with_attr_substitution() {
        let tmp = tempfile::tempdir().unwrap();
        let chapters = tmp.path().join("chapters");
        fs::create_dir(&chapters).unwrap();

        fs::write(
            tmp.path().join(".asciidoctorconfig"),
            ":chapdir: chapters\n",
        )
        .unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Doc A\n\ninclude::{chapdir}/intro.adoc[]\n").unwrap();
        fs::write(chapters.join("intro.adoc"), "== Introduction\n\nWelcome.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // Cursor on the include path, after the attribute ref
        let pos = Position {
            line: 2,
            character: 25,
        };

        let hover = resolve_hover(&content, pos, &a, Some(&ws)).unwrap();
        assert!(
            hover.contains("include::chapters/intro.adoc[]"),
            "Should show resolved path, got: {hover}"
        );
        assert!(
            hover.contains("\u{2713} File exists"),
            "Should confirm existence, got: {hover}"
        );
    }

    #[test]
    fn test_hover_include_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Doc A\n\ninclude::nonexistent.adoc[]\n").unwrap();

        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 12,
        };

        let hover = resolve_hover(&content, pos, &a, None).unwrap();
        assert!(
            hover.contains("include::nonexistent.adoc[]"),
            "Should show path, got: {hover}"
        );
        assert!(
            hover.contains("\u{2717} File not found"),
            "Should show not found, got: {hover}"
        );
    }

    // --- Image hover tests ---

    #[test]
    fn test_hover_image_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let img = tmp.path().join("photo.png");

        fs::write(&a, "= Doc A\n\nimage::photo.png[My Photo]\n").unwrap();
        fs::write(&img, "fake-png-data").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 10,
        };

        let hover = resolve_hover(&content, pos, &a, Some(&ws)).unwrap();
        assert!(
            hover.contains("image::photo.png[]"),
            "Should show image path, got: {hover}"
        );
        assert!(
            hover.contains("\u{2713} File exists"),
            "Should confirm existence, got: {hover}"
        );
    }

    #[test]
    fn test_hover_image_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Doc A\n\nimage::missing.png[Alt]\n").unwrap();

        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 10,
        };

        let hover = resolve_hover(&content, pos, &a, None).unwrap();
        assert!(
            hover.contains("image::missing.png[]"),
            "Should show path, got: {hover}"
        );
        assert!(
            hover.contains("\u{2717} File not found"),
            "Should show not found, got: {hover}"
        );
    }

    #[test]
    fn test_hover_image_with_imagesdir() {
        let tmp = tempfile::tempdir().unwrap();
        let images = tmp.path().join("assets").join("img");
        fs::create_dir_all(&images).unwrap();

        fs::write(
            tmp.path().join(".asciidoctorconfig"),
            ":imagesdir: assets/img\n",
        )
        .unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Doc A\n\nimage::logo.png[Logo]\n").unwrap();
        fs::write(images.join("logo.png"), "fake-png-data").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 10,
        };

        let hover = resolve_hover(&content, pos, &a, Some(&ws)).unwrap();
        assert!(
            hover.contains("image::assets/img/logo.png[]"),
            "Should show resolved path with imagesdir, got: {hover}"
        );
        assert!(
            hover.contains("\u{2713} File exists"),
            "Should confirm existence, got: {hover}"
        );
    }

    #[test]
    fn test_hover_anchor_returns_none() {
        let content = "= Title\n\n[[my-anchor]]\n== Section\n";
        let pos = Position {
            line: 2,
            character: 4,
        };
        assert!(resolve_hover(content, pos, Path::new("test.adoc"), None).is_none());
    }

    #[test]
    fn test_hover_plain_text_returns_none() {
        let content = "= Title\n\nJust plain text.\n";
        let pos = Position {
            line: 2,
            character: 5,
        };
        assert!(resolve_hover(content, pos, Path::new("test.adoc"), None).is_none());
    }

    #[test]
    fn test_hover_asciidoctorconfigdir_in_config_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join(".asciidoctorconfig");
        fs::write(&config, ":source-dir: {asciidoctorconfigdir}/../src\n").unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Doc\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&config).unwrap();
        // Cursor on {asciidoctorconfigdir} — starts at col 13
        let pos = Position {
            line: 0,
            character: 14,
        };

        let hover = resolve_hover(&content, pos, &config, Some(&ws));
        assert!(
            hover.is_some(),
            "Should resolve asciidoctorconfigdir in .asciidoctorconfig"
        );
        let text = hover.unwrap();
        assert!(
            text.contains(":asciidoctorconfigdir:"),
            "Should show attribute name, got: {text}"
        );
    }

    /// Regression: hover must work even when doc_path is not canonicalized
    /// (LSP handler passes raw URI path, but config_chains keys are canonical).
    #[test]
    fn test_hover_asciidoctorconfigdir_non_canonical_path() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical_root = tmp.path().canonicalize().unwrap();
        let config = canonical_root.join(".asciidoctorconfig");
        fs::write(&config, ":source-dir: {asciidoctorconfigdir}/../src\n").unwrap();
        let a = canonical_root.join("a.adoc");
        fs::write(&a, "= Doc\n").unwrap();

        let ws = Workspace::build(&canonical_root, &a).unwrap();
        let content = fs::read_to_string(&config).unwrap();

        // Simulate non-canonical path (as LSP hover handler would pass it):
        // insert a /./  segment to break exact key match
        let non_canonical_config = canonical_root.join(".").join(".asciidoctorconfig");

        let pos = Position {
            line: 0,
            character: 14,
        };

        let hover = resolve_hover(&content, pos, &non_canonical_config, Some(&ws));
        assert!(
            hover.is_some(),
            "Should resolve asciidoctorconfigdir even with non-canonical doc_path"
        );
    }

    // --- adoc-0033: image tests (unchanged) ---

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

    // --- adoc-0036: Column-finder helpers ---

    #[test]
    fn test_find_anchor_def_columns() {
        assert_eq!(
            find_anchor_def_columns("[[my-anchor]]", "my-anchor"),
            Some((2, 11))
        );
        assert_eq!(find_anchor_def_columns("[[a]]", "a"), Some((2, 3)));
        assert_eq!(
            find_anchor_def_columns("some text [[id]] more", "id"),
            Some((12, 14))
        );
        assert_eq!(find_anchor_def_columns("no anchor here", "id"), None);
    }

    #[test]
    fn test_find_anchor_ref_columns_xref_cross_doc() {
        // xref:file.adoc#my-id[text]
        let line = "See xref:file.adoc#my-id[text] for details.";
        assert_eq!(find_anchor_ref_columns(line, "my-id"), Some((19, 24)));
    }

    #[test]
    fn test_find_anchor_ref_columns_xref_same_doc() {
        // xref:my-id[text]
        let line = "See xref:my-id[text] for details.";
        assert_eq!(find_anchor_ref_columns(line, "my-id"), Some((9, 14)));
    }

    #[test]
    fn test_find_anchor_ref_columns_shorthand() {
        // <<my-id>>
        let line = "See <<my-id>> for details.";
        assert_eq!(find_anchor_ref_columns(line, "my-id"), Some((6, 11)));
    }

    #[test]
    fn test_find_anchor_ref_columns_shorthand_with_text() {
        // <<my-id,display text>>
        let line = "See <<my-id,display text>> for details.";
        assert_eq!(find_anchor_ref_columns(line, "my-id"), Some((6, 11)));
    }

    #[test]
    fn test_find_anchor_ref_columns_no_match() {
        assert_eq!(find_anchor_ref_columns("plain text", "id"), None);
    }

    #[test]
    fn test_find_anchor_ref_columns_hash_id_not_in_xref() {
        // #my-id appears after a closed xref — should not match
        let line = "See xref:other#foo[text] and some #my-id note.";
        assert_eq!(find_anchor_ref_columns(line, "my-id"), None);
    }

    #[test]
    fn test_find_anchor_ref_columns_two_xrefs_second_match() {
        // Two xrefs on same line — should find the correct one
        let line = "See xref:a.adoc#foo[x] and xref:b.adoc#bar[y].";
        assert_eq!(find_anchor_ref_columns(line, "bar"), Some((39, 42)));
    }

    #[test]
    fn test_find_attr_def_columns() {
        assert_eq!(
            find_attr_def_columns(":author: Alice", "author"),
            Some((1, 7))
        );
        assert_eq!(find_attr_def_columns(":a:", "a"), Some((1, 2)));
        assert_eq!(find_attr_def_columns("not a def", "x"), None);
        assert_eq!(find_attr_def_columns("text :name: value", "name"), None); // not at start
    }

    // --- adoc-0036: prepare_rename tests ---

    #[test]
    fn test_prepare_rename_anchor_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Title\n\n[[my-anchor]]\n== Section\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 4,
        };

        let result = prepare_rename(&content, pos, &a, Some(&ws));
        assert!(result.is_some(), "explicit anchor should be renameable");
        let r = result.unwrap();
        assert_eq!(r.placeholder, "my-anchor");
        assert_eq!(r.start_col, 2);
        assert_eq!(r.end_col, 11);
    }

    #[test]
    fn test_prepare_rename_anchor_implicit_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        // Implicit section IDs are generated from section titles.
        // We need a section without explicit anchor — the implicit ID is _section_title.
        fs::write(&a, "= Title\n\n== Section Title\n\n<<_section_title>>\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // Cursor on <<_section_title>> (shorthand xref to implicit ID)
        let pos = Position {
            line: 4,
            character: 5,
        };

        let result = prepare_rename(&content, pos, &a, Some(&ws));
        assert!(result.is_none(), "implicit section ID should be rejected");
    }

    #[test]
    fn test_prepare_rename_attribute_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Title\n:author: Alice\n\nBy {author}.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // Cursor on {author} — line 3 (0-indexed), character 4 = inside "author"
        let pos = Position {
            line: 3,
            character: 5,
        };

        let result = prepare_rename(&content, pos, &a, Some(&ws));
        assert!(result.is_some(), "attribute ref should be renameable");
        let r = result.unwrap();
        assert_eq!(r.placeholder, "author");
    }

    #[test]
    fn test_prepare_rename_include_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");
        fs::write(&a, "= Title\n\ninclude::b.adoc[]\n").unwrap();
        fs::write(&b, "== Chapter\n").unwrap();

        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 12,
        };

        let result = prepare_rename(&content, pos, &a, None);
        assert!(result.is_none(), "include should not be renameable");
    }

    // --- adoc-0036: resolve_rename tests ---

    #[test]
    fn test_resolve_rename_anchor_single_file() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(
            &a,
            "= Title\n\n[[my-anchor]]\n== Section\n\nSee <<my-anchor>>.\n",
        )
        .unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 4,
        };

        let result = resolve_rename(&content, pos, &a, Some(&ws), &HashMap::new());
        assert!(result.is_some());
        let edits = result.unwrap();
        let a_canon = a.canonicalize().unwrap();
        let file_edits = edits.get(&a_canon).unwrap();
        // Should have 2 edits: definition [[my-anchor]] and reference <<my-anchor>>
        assert_eq!(file_edits.len(), 2, "expected 2 edits, got: {file_edits:?}");
    }

    #[test]
    fn test_resolve_rename_anchor_cross_file() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");
        fs::write(
            &a,
            "= Title\n\nSee xref:b.adoc#target[link].\n\ninclude::b.adoc[]\n",
        )
        .unwrap();
        fs::write(&b, "[[target]]\n== Target Section\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // Cursor on xref target
        let pos = Position {
            line: 2,
            character: 18,
        };

        let result = resolve_rename(&content, pos, &a, Some(&ws), &HashMap::new());
        assert!(result.is_some());
        let edits = result.unwrap();
        // Should have edits in both files
        assert!(
            edits.len() >= 2,
            "expected edits in 2 files, got: {}",
            edits.len()
        );
    }

    #[test]
    fn test_resolve_rename_anchor_with_file_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");
        fs::write(
            &a,
            "= Title\n\nSee xref:b.adoc#my-id[link].\n\ninclude::b.adoc[]\n",
        )
        .unwrap();
        fs::write(&b, "[[my-id]]\n== Section\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 20,
        };

        let result = resolve_rename(&content, pos, &a, Some(&ws), &HashMap::new());
        assert!(result.is_some());
        let edits = result.unwrap();
        let a_canon = a.canonicalize().unwrap();
        let a_edits = edits.get(&a_canon).unwrap();
        // Verify the xref edit targets only the anchor part after #
        let (_, start, end) = a_edits[0];
        let a_content = fs::read_to_string(&a).unwrap();
        let line_text = a_content.lines().nth(2).unwrap(); // line 3 (0-indexed line 2)
        assert_eq!(&line_text[start..end], "my-id");
    }

    #[test]
    fn test_resolve_rename_attribute() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Title\n:myattr: value\n\nUsing {myattr} here.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // Cursor on {myattr}
        let pos = Position {
            line: 3,
            character: 8,
        };

        let result = resolve_rename(&content, pos, &a, Some(&ws), &HashMap::new());
        assert!(result.is_some());
        let edits = result.unwrap();
        let a_canon = a.canonicalize().unwrap();
        let file_edits = edits.get(&a_canon).unwrap();
        // Should have 2 edits: definition :myattr: and reference {myattr}
        assert_eq!(file_edits.len(), 2, "expected 2 edits, got: {file_edits:?}");
    }

    #[test]
    fn test_resolve_rename_attribute_cross_file() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(".asciidoctorconfig"),
            ":shared-attr: hello\n",
        )
        .unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Title\n\nValue is {shared-attr}.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 12,
        };

        let result = resolve_rename(&content, pos, &a, Some(&ws), &HashMap::new());
        assert!(result.is_some());
        let edits = result.unwrap();
        // Should have edits in both .asciidoctorconfig and a.adoc
        assert!(
            edits.len() >= 2,
            "expected edits in 2 files, got: {}",
            edits.len()
        );
    }

    // --- Bug investigation: prepare_rename end-to-end column check ---

    #[test]
    fn test_prepare_rename_anchor_lsp_range() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Title\n\n[[my-anchor]]\n== Section\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 4,
        };

        let r = prepare_rename(&content, pos, &a, Some(&ws)).unwrap();
        // Check raw PrepareRenameResult
        assert_eq!(r.placeholder, "my-anchor");
        assert_eq!(r.line, 3); // 1-indexed
        assert_eq!(r.start_col, 2); // after [[
        assert_eq!(r.end_col, 11); // end of "my-anchor"

        // Simulate what main.rs handler does
        let start = crate::positions::to_lsp_position(r.line, r.start_col, &content);
        let end = crate::positions::to_lsp_position(r.line, r.end_col, &content);

        assert_eq!(start.line, 2, "start line should be 0-indexed line 2");
        assert_eq!(
            start.character, 2,
            "start character should be 2 (after [[), got {}",
            start.character
        );
        assert_eq!(end.line, 2, "end line should be 0-indexed line 2");
        assert_eq!(
            end.character, 11,
            "end character should be 11, got {}",
            end.character
        );

        // Verify the text at the range matches the placeholder
        let line_text = content.lines().nth(2).unwrap();
        assert_eq!(
            &line_text[start.character as usize..end.character as usize],
            "my-anchor"
        );
    }

    #[test]
    fn test_prepare_rename_attr_def_lsp_range() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Title\n:author: Alice\n\nBy {author}.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // Cursor on :author: definition
        let pos = Position {
            line: 1,
            character: 3,
        };

        let r = prepare_rename(&content, pos, &a, Some(&ws)).unwrap();
        assert_eq!(r.placeholder, "author");

        let start = crate::positions::to_lsp_position(r.line, r.start_col, &content);
        let end = crate::positions::to_lsp_position(r.line, r.end_col, &content);

        assert_eq!(start.line, 1);
        assert_ne!(start.character, 0, "start character must not be 0");
        assert_eq!(
            start.character, 1,
            "start should be 1 (after leading colon)"
        );
        assert_eq!(end.character, 7, "end should be 7 (after 'author')");

        let line_text = content.lines().nth(1).unwrap();
        assert_eq!(
            &line_text[start.character as usize..end.character as usize],
            "author"
        );
    }

    #[test]
    fn test_prepare_rename_attr_ref_lsp_range() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Title\n:author: Alice\n\nBy {author}.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // Cursor on {author} reference — line 3 (0-indexed), character 5
        let pos = Position {
            line: 3,
            character: 5,
        };

        let r = prepare_rename(&content, pos, &a, Some(&ws)).unwrap();
        assert_eq!(r.placeholder, "author");

        let start = crate::positions::to_lsp_position(r.line, r.start_col, &content);
        let end = crate::positions::to_lsp_position(r.line, r.end_col, &content);

        assert_eq!(start.line, 3);
        assert_ne!(start.character, 0, "start character must not be 0");

        let line_text = content.lines().nth(3).unwrap();
        assert_eq!(
            &line_text[start.character as usize..end.character as usize],
            "author",
            "range should cover exactly the attribute name"
        );
    }

    #[test]
    fn test_prepare_rename_xref_shorthand_lsp_range() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(
            &a,
            "= Title\n\n[[my-anchor]]\n== Section\n\nSee <<my-anchor>>.\n",
        )
        .unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // Cursor on <<my-anchor>> — line 5 (0-indexed), character 7
        let pos = Position {
            line: 5,
            character: 7,
        };

        let r = prepare_rename(&content, pos, &a, Some(&ws)).unwrap();
        assert_eq!(r.placeholder, "my-anchor");

        let start = crate::positions::to_lsp_position(r.line, r.start_col, &content);
        let end = crate::positions::to_lsp_position(r.line, r.end_col, &content);

        assert_eq!(start.line, 5);
        assert_ne!(start.character, 0, "start character must not be 0");

        let line_text = content.lines().nth(5).unwrap();
        assert_eq!(
            &line_text[start.character as usize..end.character as usize],
            "my-anchor",
            "range should cover exactly the anchor id in the xref"
        );
    }

    #[test]
    fn test_resolve_rename_anchor_edit_columns() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(
            &a,
            "= Title\n\n[[my-anchor]]\n== Section\n\nSee <<my-anchor>>.\n",
        )
        .unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 4,
        };

        let edits = resolve_rename(&content, pos, &a, Some(&ws), &HashMap::new()).unwrap();
        let a_canon = a.canonicalize().unwrap();
        let file_edits = edits.get(&a_canon).unwrap();
        assert_eq!(file_edits.len(), 2);

        let file_content = fs::read_to_string(&a_canon).unwrap();
        for (line, start_col, end_col) in file_edits {
            let start = crate::positions::to_lsp_position(*line, *start_col, &file_content);
            let end = crate::positions::to_lsp_position(*line, *end_col, &file_content);

            assert_ne!(
                start.character, 0,
                "edit start must not be column 0, line {line}"
            );
            assert_ne!(
                start.character, end.character,
                "edit range must not be empty, line {line}"
            );

            let line_text = file_content.lines().nth(start.line as usize).unwrap();
            assert_eq!(
                &line_text[start.character as usize..end.character as usize],
                "my-anchor",
                "edit should cover exactly the anchor id, line {line}"
            );
        }
    }

    #[test]
    fn test_resolve_rename_attribute_edit_columns() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Title\n:myattr: value\n\nUsing {myattr} here.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();
        let content = fs::read_to_string(&a).unwrap();
        // Cursor on {myattr}
        let pos = Position {
            line: 3,
            character: 8,
        };

        let edits = resolve_rename(&content, pos, &a, Some(&ws), &HashMap::new()).unwrap();
        let a_canon = a.canonicalize().unwrap();
        let file_edits = edits.get(&a_canon).unwrap();
        assert_eq!(
            file_edits.len(),
            2,
            "expected 2 edits (def + ref), got: {file_edits:?}"
        );

        let file_content = fs::read_to_string(&a_canon).unwrap();
        for (line, start_col, end_col) in file_edits {
            let start = crate::positions::to_lsp_position(*line, *start_col, &file_content);
            let end = crate::positions::to_lsp_position(*line, *end_col, &file_content);

            assert_ne!(
                start.character, 0,
                "edit start must not be column 0, line {line}"
            );
            assert_ne!(
                start.character, end.character,
                "edit range must not be empty, line {line}"
            );

            let line_text = file_content.lines().nth(start.line as usize).unwrap();
            assert_eq!(
                &line_text[start.character as usize..end.character as usize],
                "myattr",
                "edit should cover exactly the attribute name, line {line}"
            );
        }
    }

    // --- Document-Store desync regression tests ---

    #[test]
    fn test_resolve_rename_anchor_uses_overrides_not_disk() {
        // After a rename, disk still has old content but the editor buffer
        // (overrides) has the new content. A second rename must use the
        // override content, not stale disk content.
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");

        // Disk: original content with "old-id"
        fs::write(&a, "= Title\n\n[[old-id]]\n== Section\n\nSee <<old-id>>.\n").unwrap();

        // Simulate post-rename state: workspace rebuilt with new content
        let new_content = "= Title\n\n[[new-id]]\n== Section\n\nSee <<new-id>>.\n";
        let a_canon = a.canonicalize().unwrap();
        let mut overrides = HashMap::new();
        overrides.insert(a_canon.clone(), new_content.to_string());

        let ws = Workspace::build_with_overrides(tmp.path(), &a, &overrides).unwrap();

        let pos = Position {
            line: 2,
            character: 4,
        };

        // Without overrides: would fail because disk has "old-id" but we search "new-id"
        let result_no_override =
            resolve_rename(new_content, pos, &a_canon, Some(&ws), &HashMap::new());
        assert!(
            result_no_override.is_none(),
            "without overrides, stale disk content should cause column search to miss"
        );

        // With overrides: should find both definition and reference
        let result = resolve_rename(new_content, pos, &a_canon, Some(&ws), &overrides);
        assert!(result.is_some(), "with overrides, rename should succeed");
        let edits = result.unwrap();
        let file_edits = edits.get(&a_canon).unwrap();
        assert_eq!(
            file_edits.len(),
            2,
            "expected 2 edits (def + ref), got: {file_edits:?}"
        );
    }

    #[test]
    fn test_resolve_rename_anchor_cross_file_uses_overrides() {
        // Cross-file rename after first rename — other file has new content
        // in editor but old content on disk.
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        // Disk: original content
        fs::write(
            &a,
            "= Title\n\nSee xref:b.adoc#old-id[link].\n\ninclude::b.adoc[]\n",
        )
        .unwrap();
        fs::write(&b, "[[old-id]]\n== Target Section\n").unwrap();

        // Simulate post-rename: editor buffers have new content
        let a_canon = a.canonicalize().unwrap();
        let b_canon = b.canonicalize().unwrap();
        let new_a = "= Title\n\nSee xref:b.adoc#new-id[link].\n\ninclude::b.adoc[]\n";
        let new_b = "[[new-id]]\n== Target Section\n";

        let mut overrides = HashMap::new();
        overrides.insert(a_canon.clone(), new_a.to_string());
        overrides.insert(b_canon.clone(), new_b.to_string());

        let ws = Workspace::build_with_overrides(tmp.path(), &a, &overrides).unwrap();

        // Cursor on xref target in a.adoc
        let pos = Position {
            line: 2,
            character: 20,
        };

        let result = resolve_rename(new_a, pos, &a_canon, Some(&ws), &overrides);
        assert!(
            result.is_some(),
            "cross-file rename with overrides should work"
        );
        let edits = result.unwrap();

        // Should have edits in both files
        let a_edits = edits.get(&a_canon).expect("should have edits for a.adoc");
        let b_edits = edits.get(&b_canon).expect("should have edits for b.adoc");
        assert_eq!(a_edits.len(), 1, "a.adoc: 1 xref reference");
        assert_eq!(b_edits.len(), 1, "b.adoc: 1 anchor definition");
    }

    #[test]
    fn test_resolve_rename_attribute_uses_overrides() {
        // Attribute rename with stale disk content.
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");

        // Disk: original content
        fs::write(&a, "= Title\n:old-attr: value\n\nText {old-attr} here.\n").unwrap();

        // Simulate post-rename
        let new_content = "= Title\n:new-attr: value\n\nText {new-attr} here.\n";
        let a_canon = a.canonicalize().unwrap();
        let mut overrides = HashMap::new();
        overrides.insert(a_canon.clone(), new_content.to_string());

        let ws = Workspace::build_with_overrides(tmp.path(), &a, &overrides).unwrap();

        // Cursor on attribute reference
        let pos = Position {
            line: 3,
            character: 7,
        };

        let result = resolve_rename(new_content, pos, &a_canon, Some(&ws), &overrides);
        assert!(
            result.is_some(),
            "attribute rename with overrides should work"
        );
        let edits = result.unwrap();
        let file_edits = edits.get(&a_canon).unwrap();
        assert_eq!(
            file_edits.len(),
            2,
            "expected 2 edits (def + ref), got: {file_edits:?}"
        );
    }
}
