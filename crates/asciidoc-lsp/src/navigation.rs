use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use tower_lsp_server::ls_types::Position;

use asciidoc_core::attributes::{AttributeHit, AttributeLayer, AttributeSource};
use asciidoc_core::extractors::find_child_text;
use asciidoc_core::types::{AnchorKind, DiagnosticKind, FileLocation, ResolvedXref};
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
    /// `image::path[]` block macro or `image:path[]` inline macro
    Image { path: String, inline: bool },
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
pub(crate) fn find_node_at_position(content: &str, pos: Position) -> Option<NodeInfo> {
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

    // Try inline grammar (xrefs, shorthand xrefs, inline images).
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
            let target = resolve_include_target(path, doc_path, workspace)?;
            // Require an existing on-disk file.
            let target_canon = target.canonicalize().ok()?;
            Some((target_canon, 1))
        }
        NodeKind::Xref { ref target } | NodeKind::ShorthandXref { ref target } => {
            let ws = workspace?;
            let resolved = resolve_xref_target(ws, doc_path, target, node_info.line).ok()?;
            Some((resolved.file, resolved.line))
        }
        NodeKind::AttributeRef { ref name } => {
            let ws = workspace?;
            let source = ws.attributes().resolve_source(doc_path, name)?;
            Some((source.file, source.line))
        }
        NodeKind::Image { ref path, .. } => {
            let ImageTarget::Local { path: target, .. } =
                resolve_image_target(path, doc_path, workspace)
            else {
                return None;
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
/// When `include_declaration` is true and cursor is on a `{name}` reference,
/// the definition location is included in the results.
pub fn resolve_references(
    content: &str,
    pos: Position,
    doc_path: &Path,
    workspace: Option<&Workspace>,
    include_declaration: bool,
) -> Option<Vec<FileLocation>> {
    let node_info = find_node_at_position(content, pos)?;
    let ws = workspace?;

    let mut results = match &node_info.kind {
        NodeKind::AttributeRef { name } | NodeKind::AttributeDef { name } => {
            let mut r = ws.find_all_attribute_usages(name);
            if include_declaration && matches!(node_info.kind, NodeKind::AttributeRef { .. }) {
                if let Some(source) = ws.attributes().resolve_source(doc_path, name) {
                    r.push(FileLocation::new(source.file, source.line, 0, 0));
                }
            }
            r
        }
        NodeKind::Include { path } => {
            let target = asciidoc_core::paths::resolve_doc_target(doc_path, path, ws.attributes())?;
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
                if let Some(loc) = ws.reference_index().lookup(id) {
                    r.push(FileLocation::new(loc.file.clone(), loc.line, 0, 0));
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
                if let Some(loc) = ws.reference_index().lookup(anchor_id) {
                    r.push(FileLocation::new(loc.file.clone(), loc.line, 0, 0));
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
    results.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    results.dedup_by(|a, b| a.path == b.path && a.line == b.line);

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
            let value = ws.attributes().resolve(doc_path, name)?;
            let mut text = format!(":{name}: {value}");
            if let Some(source) = ws.attributes().resolve_source(doc_path, name) {
                let display_path = file_display_name(&source.file);
                let _ = write!(text, "\n\nDefined in {display_path}:{}", source.line);
            }
            Some(text)
        }
        NodeKind::AttributeDef { name } => {
            let ws = workspace?;
            let value = ws.attributes().resolve(doc_path, name)?;
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
                .reference_index()
                .resolve_xref(
                    &xref,
                    doc_path,
                    ws.include_graph(),
                    ws.documents(),
                    ws.attributes(),
                )
                .ok()?;

            // Find section title at the resolved location
            let section_title = ws.documents().get(&resolved.file).and_then(|doc| {
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
            let resolved_path = if let Some(ws) = workspace {
                ws.attributes().substitute(doc_path, path)
            } else {
                path.clone()
            };
            let target = if let Some(ws) = workspace {
                asciidoc_core::paths::resolve_doc_target(doc_path, path, ws.attributes())?
            } else {
                doc_path.parent()?.join(&resolved_path)
            };

            let mut text = format!("include::{resolved_path}[]");
            if target.exists() {
                let section_count = workspace
                    .and_then(|ws| ws.documents().get(&target.canonicalize().ok()?))
                    .map_or(0, |doc| doc.sections.len());
                let _ = write!(text, "\n\n\u{2713} File exists | {section_count} sections");
            } else {
                text.push_str("\n\n\u{2717} File not found");
            }
            Some(text)
        }
        NodeKind::Image { path, inline } => {
            let macro_form = if *inline { "image:" } else { "image::" };
            match resolve_image_target(path, doc_path, workspace) {
                ImageTarget::Remote { url } => Some(format!("{macro_form}{url}[]")),
                ImageTarget::Local {
                    path: target,
                    display,
                } => {
                    let mut text = format!("{macro_form}{display}[]");
                    if target.exists() {
                        text.push_str("\n\n\u{2713} File exists");
                    } else {
                        text.push_str("\n\n\u{2717} File not found");
                    }
                    Some(text)
                }
            }
        }
        NodeKind::Anchor { .. } => None,
    }
}

/// Resolve the base directory and imagesdir for image paths.
///
/// Returns `(base_dir, resolved_imagesdir)` where `base_dir` is `doc_dir / imagesdir`
/// (or just `doc_dir` if imagesdir is empty/unset).
pub(crate) fn resolve_image_base_dir(
    doc_path: &Path,
    workspace: Option<&Workspace>,
) -> (PathBuf, String) {
    let doc_dir = doc_path.parent().unwrap_or(doc_path);
    let imagesdir_raw = workspace
        .and_then(|ws| ws.attributes().resolve(doc_path, "imagesdir"))
        .unwrap_or("");
    let imagesdir = if let Some(ws) = workspace {
        ws.attributes().substitute(doc_path, imagesdir_raw)
    } else {
        imagesdir_raw.to_string()
    };
    let base = if imagesdir.is_empty() {
        doc_dir.to_path_buf()
    } else {
        doc_dir.join(&imagesdir)
    };
    (base, imagesdir)
}

/// Where an image macro points after attribute substitution and `imagesdir`.
enum ImageTarget {
    /// A file: `path` is absolute, `display` is the human-readable path
    /// including imagesdir.
    Local { path: PathBuf, display: String },
    /// An `http(s)` URL, from the macro target or from `imagesdir`.
    Remote { url: String },
}

fn is_remote_url(target: &str) -> bool {
    target.starts_with("http://") || target.starts_with("https://")
}

/// Resolve an image macro's path to a file or a remote URL.
fn resolve_image_target(
    image_path: &str,
    doc_path: &Path,
    workspace: Option<&Workspace>,
) -> ImageTarget {
    let resolved_path = if let Some(ws) = workspace {
        ws.attributes().substitute(doc_path, image_path)
    } else {
        image_path.to_string()
    };
    let (base, imagesdir) = resolve_image_base_dir(doc_path, workspace);
    let display = if imagesdir.is_empty() || is_remote_url(&resolved_path) {
        resolved_path.clone()
    } else {
        format!("{imagesdir}/{resolved_path}")
    };
    if is_remote_url(&display) {
        return ImageTarget::Remote { url: display };
    }
    ImageTarget::Local {
        path: base.join(&resolved_path),
        display,
    }
}

/// Resolve an include macro's path to a filesystem target, without checking
/// that it exists. Shared by go-to-definition and the explain report.
fn resolve_include_target(
    include_path: &str,
    doc_path: &Path,
    workspace: Option<&Workspace>,
) -> Option<PathBuf> {
    if let Some(ws) = workspace {
        asciidoc_core::paths::resolve_doc_target(doc_path, include_path, ws.attributes())
    } else {
        doc_path.parent().map(|dir| dir.join(include_path))
    }
}

/// Resolve an xref target through the workspace reference index.
/// Shared by go-to-definition and the explain report.
fn resolve_xref_target(
    ws: &Workspace,
    doc_path: &Path,
    target: &str,
    line: usize,
) -> Result<ResolvedXref, DiagnosticKind> {
    let xref = asciidoc_core::types::Xref {
        target: target.to_string(),
        display_text: None,
        line,
    };
    ws.reference_index().resolve_xref(
        &xref,
        doc_path,
        ws.include_graph(),
        ws.documents(),
        ws.attributes(),
    )
}

/// Build a debug report of what go-to-definition and hover see at `pos`.
///
/// `lookup_path` is the path the handlers resolve with; pass it exactly as
/// they do, so a path-key mismatch in the workspace shows up in the report.
/// `raw_path` is the path of the client's URI, reported next to its canonical
/// form. Top-level fields: `request`, `node`, `resolution`,
/// `known_attributes`.
pub fn explain_position(
    content: &str,
    pos: Position,
    raw_path: &Path,
    lookup_path: &Path,
    workspace: Option<&Workspace>,
) -> Value {
    let node_info = find_node_at_position(content, pos);
    let node = node_info.as_ref().map_or(Value::Null, explain_node);
    let resolution = node_info.as_ref().map_or(Value::Null, |info| {
        explain_resolution(info, lookup_path, workspace)
    });
    let known_attributes: Vec<Value> = workspace
        .map(|ws| {
            ws.attributes()
                .known_attributes(lookup_path)
                .into_iter()
                .map(|k| {
                    json!({
                        "name": k.name,
                        "value": k.value,
                        "layer": layer_name(k.layer),
                        "source": source_json(k.source.as_ref()),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    json!({
        "request": explain_request(raw_path, lookup_path, workspace),
        "node": node,
        "resolution": resolution,
        "known_attributes": known_attributes,
    })
}

fn explain_request(raw_path: &Path, lookup_path: &Path, workspace: Option<&Workspace>) -> Value {
    let canonical = raw_path.canonicalize();
    let in_documents = |p: &Path| workspace.is_some_and(|ws| ws.documents().contains_key(p));
    let (canonical_path, canonicalize_error, paths_differ, in_canonical) = match &canonical {
        Ok(c) => (
            Value::String(path_string(c)),
            Value::Null,
            c != raw_path,
            in_documents(c),
        ),
        Err(e) => (Value::Null, Value::String(e.to_string()), false, false),
    };
    json!({
        "raw_path": path_string(raw_path),
        "canonical_path": canonical_path,
        "lookup_path": path_string(lookup_path),
        "canonicalize_error": canonicalize_error,
        "paths_differ": paths_differ,
        "workspace_root": workspace.map(|ws| path_string(ws.root())),
        "in_documents_raw": in_documents(raw_path),
        "in_documents_canonical": in_canonical,
    })
}

fn explain_node(info: &NodeInfo) -> Value {
    let fields = match &info.kind {
        NodeKind::Include { path } | NodeKind::Image { path, .. } => json!({ "path": path }),
        NodeKind::Xref { target } | NodeKind::ShorthandXref { target } => {
            json!({ "target": target })
        }
        NodeKind::AttributeRef { name } | NodeKind::AttributeDef { name } => {
            json!({ "name": name })
        }
        NodeKind::Anchor { id } => json!({ "id": id }),
    };
    json!({ "kind": node_kind_name(&info.kind), "line": info.line, "fields": fields })
}

/// Stable report name of a node kind, independent of Rust's `Debug`.
fn node_kind_name(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::Include { .. } => "include",
        NodeKind::Xref { .. } => "xref",
        NodeKind::ShorthandXref { .. } => "shorthand_xref",
        NodeKind::AttributeRef { .. } => "attribute_ref",
        NodeKind::AttributeDef { .. } => "attribute_def",
        NodeKind::Anchor { .. } => "anchor",
        NodeKind::Image { .. } => "image",
    }
}

fn explain_resolution(info: &NodeInfo, doc_path: &Path, workspace: Option<&Workspace>) -> Value {
    let no_workspace = || json!({ "error": "no workspace" });
    match &info.kind {
        NodeKind::AttributeRef { name } | NodeKind::AttributeDef { name } => {
            workspace.map_or_else(no_workspace, |ws| explain_attribute(ws, doc_path, name))
        }
        NodeKind::Xref { target } | NodeKind::ShorthandXref { target } => workspace
            .map_or_else(no_workspace, |ws| {
                explain_xref(ws, doc_path, target, info.line)
            }),
        NodeKind::Anchor { id } => workspace.map_or_else(no_workspace, |ws| {
            let definition = ws.reference_index().lookup(id).map(|loc| {
                json!({
                    "file": path_string(&loc.file),
                    "line": loc.line,
                    "kind": anchor_kind_name(loc.kind),
                })
            });
            json!({ "definition": definition })
        }),
        NodeKind::Include { path } => {
            let target = resolve_include_target(path, doc_path, workspace);
            // Same existence gate as go-to-definition: the path gd returns.
            let canonical_target = target.as_deref().and_then(|t| t.canonicalize().ok());
            json!({
                "target": target.as_deref().map(path_string),
                "canonical_target": canonical_target.as_deref().map(path_string),
                "exists": target.as_deref().is_some_and(Path::exists),
            })
        }
        NodeKind::Image { path, .. } => {
            match resolve_image_target(path, doc_path, workspace) {
                ImageTarget::Remote { url } => json!({
                    "remote": true,
                    "target": null,
                    "canonical_target": null,
                    "display": url,
                    "exists": null,
                }),
                ImageTarget::Local {
                    path: target,
                    display,
                } => {
                    // Same existence gate as go-to-definition: the path gd returns.
                    let canonical_target = target.canonicalize().ok();
                    json!({
                        "remote": false,
                        "target": path_string(&target),
                        "canonical_target": canonical_target.as_deref().map(path_string),
                        "display": display,
                        "exists": target.exists(),
                    })
                }
            }
        }
    }
}

fn explain_attribute(ws: &Workspace, doc_path: &Path, name: &str) -> Value {
    let registry = ws.attributes();
    let explanation = registry.explain(doc_path, name);
    json!({
        "name": name,
        "value": registry.resolve(doc_path, name),
        "winner": explanation.winner.map(layer_name),
        "per_file": {
            "registered": explanation.per_file.registered,
            "hit": hit_json(explanation.per_file.hit.as_ref()),
        },
        "config": {
            "dir": explanation.config.dir.as_deref().map(path_string),
            "chain_cached": explanation.config.chain_cached,
            "hit": hit_json(explanation.config.hit.as_ref()),
        },
        "builtin": explanation.builtin,
    })
}

fn explain_xref(ws: &Workspace, doc_path: &Path, target: &str, line: usize) -> Value {
    match resolve_xref_target(ws, doc_path, target, line) {
        Ok(resolved) => json!({
            "resolved": { "file": path_string(&resolved.file), "line": resolved.line },
            "error": null,
        }),
        Err(e) => json!({ "resolved": null, "error": e.to_string() }),
    }
}

fn hit_json(hit: Option<&AttributeHit>) -> Value {
    hit.map_or(
        Value::Null,
        |h| json!({ "value": h.value, "source": source_json(h.source.as_ref()) }),
    )
}

fn source_json(source: Option<&AttributeSource>) -> Value {
    source.map_or(
        Value::Null,
        |s| json!({ "file": path_string(&s.file), "line": s.line }),
    )
}

/// Stable report name of an attribute layer, independent of Rust's `Debug`.
fn layer_name(layer: AttributeLayer) -> &'static str {
    match layer {
        AttributeLayer::PerFile => "per_file",
        AttributeLayer::Config => "config",
        AttributeLayer::Builtin => "builtin",
    }
}

/// Stable report name of an anchor kind, independent of Rust's `Debug`.
fn anchor_kind_name(kind: AnchorKind) -> &'static str {
    match kind {
        AnchorKind::Explicit => "explicit",
        AnchorKind::ImplicitSection => "implicit_section",
        AnchorKind::BlockId => "block_id",
    }
}

fn path_string(path: &Path) -> String {
    path.display().to_string()
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
                        kind: NodeKind::Image {
                            path,
                            inline: false,
                        },
                        line: n.start_position().row + 1,
                    });
                }
            }
        }
        current = n.parent();
    }

    None
}

/// Check if the node at the given point is inside an xref (`inline_macro` or
/// `<<>>`) or an inline image.
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
                if macro_name == "image" {
                    let path = find_child_text(n, "target", source).unwrap_or_default();
                    return Some(NodeInfo {
                        kind: NodeKind::Image { path, inline: true },
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
                if let Some(loc) = ws.reference_index().lookup(id) {
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
                if let Some(loc) = ws.reference_index().lookup(anchor_id) {
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
    overrides: &HashMap<PathBuf, Arc<String>>,
) -> Option<RenameEdits> {
    let node_info = find_node_at_position(content, pos)?;
    let ws = workspace?;

    match &node_info.kind {
        NodeKind::AttributeRef { name } | NodeKind::AttributeDef { name } => {
            let mut edits: HashMap<PathBuf, Vec<(usize, usize, usize)>> = HashMap::new();

            // Attribute references: {name} → start_col+1 to end_col-1 (inside braces)
            let refs = ws.find_all_attribute_usages(name);
            for loc in refs {
                edits
                    .entry(loc.path)
                    .or_default()
                    .push((loc.line, loc.col + 1, loc.end_col - 1));
            }

            // Attribute definition: :name: → find columns via text search
            if let Some(source) = ws.attributes().resolve_source(doc_path, name) {
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
    overrides: &HashMap<PathBuf, Arc<String>>,
) -> Option<RenameEdits> {
    let mut edits: HashMap<PathBuf, Vec<(usize, usize, usize)>> = HashMap::new();
    let mut file_cache: HashMap<PathBuf, String> = HashMap::new();

    // Anchor definition
    if let Some(loc) = ws.reference_index().lookup(anchor_id) {
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
    for (dup_id, dup_loc) in ws.reference_index().duplicates() {
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
    for loc in refs {
        let file_content = read_file_cached(&mut file_cache, &loc.path, overrides);
        let line_text = file_content
            .lines()
            .nth(loc.line.saturating_sub(1))
            .unwrap_or_default();
        if let Some((start, end)) = find_anchor_ref_columns(line_text, anchor_id) {
            edits
                .entry(loc.path)
                .or_default()
                .push((loc.line, start, end));
        }
    }

    if edits.is_empty() {
        None
    } else {
        Some(edits)
    }
}

/// Read file content, preferring in-memory overrides over disk.
fn read_content(path: &Path, overrides: &HashMap<PathBuf, Arc<String>>) -> String {
    if let Some(content) = overrides.get(path) {
        return content.to_string();
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
    overrides: &HashMap<PathBuf, Arc<String>>,
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
    use rstest::rstest;
    use std::fs;

    // --- Task 4.1: find_node_at_position tests ---

    #[rstest]
    #[case(
        "include with line check",
        "= Title\n\ninclude::chapters/intro.adoc[]\n\nSome text.\n",
        2, 12,
        NodeKind::Include { path: "chapters/intro.adoc".into() },
        Some(3)
    )]
    #[case(
        "include at start of line",
        "= Title\n\ninclude::sub.adoc[]\n",
        2, 0,
        NodeKind::Include { path: "sub.adoc".into() },
        None
    )]
    #[case(
        "include cursor on path",
        "= Title\n\ninclude::sub.adoc[]\n",
        2, 12,
        NodeKind::Include { path: "sub.adoc".into() },
        None
    )]
    #[case(
        "xref long form",
        "= Title\n\nSee xref:file.adoc#anchor[Link Text] for details.\n",
        2, 12,
        NodeKind::Xref { target: "file.adoc#anchor".into() },
        None
    )]
    #[case(
        "shorthand xref",
        "= Title\n\nSee <<my-section,Section Title>> for details.\n",
        2, 8,
        NodeKind::ShorthandXref { target: "my-section".into() },
        None
    )]
    fn test_find_node_some(
        #[case] _name: &str,
        #[case] content: &str,
        #[case] line: u32,
        #[case] character: u32,
        #[case] expected_kind: NodeKind,
        #[case] expected_line: Option<usize>,
    ) {
        let pos = Position { line, character };
        let info = find_node_at_position(content, pos).unwrap();
        assert_eq!(info.kind, expected_kind);
        if let Some(exp_line) = expected_line {
            assert_eq!(info.line, exp_line);
        }
    }

    #[rstest]
    #[case("plain text", "= Title\n\nJust plain text.\n", 2, 5)]
    #[case("section heading", "= Title\n\n== Chapter\n", 2, 5)]
    fn test_find_node_none(
        #[case] _name: &str,
        #[case] content: &str,
        #[case] line: u32,
        #[case] character: u32,
    ) {
        let pos = Position { line, character };
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

        let ws = Workspace::build(tmp.path(), &a);
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

    // adoc-0125: xrefs in default (`|`) and AsciiDoc (`a|`) table cells.
    #[rstest]
    #[case::default_cell("| See xref:b.adoc#my-anchor[link].", 12)]
    #[case::asciidoc_cell("a| See xref:b.adoc#my-anchor[link].", 13)]
    #[case::second_cell_in_row("| A | xref:b.adoc#my-anchor[link]", 12)]
    #[case::shorthand("| See <<my-anchor>>.", 10)]
    #[case::second_line_of_cell("| First line\nxref:b.adoc#my-anchor[link]", 6)]
    fn test_goto_def_xref_in_table_cell(#[case] row: &str, #[case] character: u32) {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        let b = root.join("b.adoc");
        let content = format!("= Doc A\n\n|===\n{row}\n|===\n\ninclude::b.adoc[]\n");
        fs::write(&a, &content).unwrap();
        fs::write(&b, "[[my-anchor]]\n== Chapter B\n\nContent.\n").unwrap();

        let ws = Workspace::build(&root, &a);
        let xref_line = content
            .lines()
            .position(|l| l.contains("my-anchor"))
            .unwrap();
        let pos = Position {
            line: u32::try_from(xref_line).unwrap(),
            character,
        };

        let (target, line) = resolve_goto_definition(&content, pos, &a, Some(&ws)).unwrap();
        assert_eq!(target, b);
        assert_eq!(line, 1);
    }

    #[test]
    fn test_goto_def_xref_shorthand() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(&a, "= Doc A\n\nSee <<my-anchor>>.\n\ninclude::b.adoc[]\n").unwrap();
        fs::write(&b, "[[my-anchor]]\n== Chapter B\n\nContent.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
        let content = fs::read_to_string(&a).unwrap();
        // cursor on the page break line
        let pos = Position {
            line: 2,
            character: 1,
        };

        assert!(resolve_goto_definition(&content, pos, &a, Some(&ws)).is_none());
    }

    #[test]
    fn test_goto_def_xref_shorthand_with_display_text() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(
            &a,
            "= Doc A\n\nSee <<my-anchor,custom text>>.\n\ninclude::b.adoc[]\n",
        )
        .unwrap();
        fs::write(&b, "[[my-anchor]]\n== Chapter B\n\nContent.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a);
        let content = fs::read_to_string(&a).unwrap();
        // character 8 = inside "my-anchor" before the comma
        let pos = Position {
            line: 2,
            character: 8,
        };

        let (target, line) = resolve_goto_definition(&content, pos, &a, Some(&ws)).unwrap();
        assert_eq!(target, b.canonicalize().unwrap());
        assert_eq!(line, 1);
    }

    // --- Task 3.1: resolve_goto_definition for attribute references ---

    #[test]
    fn test_goto_def_attribute_doc_header() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Doc A\n:author: Alice\n\nWritten by {author}.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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
        let has_config = with.iter().any(|loc| loc.path == config_path);
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

        let ws = Workspace::build(tmp.path(), &index);

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
        let files: Vec<_> = results.iter().map(|loc| loc.path.clone()).collect();
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

        let ws = Workspace::build(tmp.path(), &a);

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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &b);
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

        let ws = Workspace::build(tmp.path(), &b);
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

        let ws = Workspace::build(tmp.path(), &index);
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

        let ws = Workspace::build(tmp.path(), &a);
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
        let files: Vec<_> = results.iter().map(|loc| loc.path.clone()).collect();
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
                path: "images/diagram.png".into(),
                inline: false
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
                path: "logo.png".into(),
                inline: false
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

    #[rstest]
    #[case::mid_line("= Doc A\n:imagesdir: img\n\nText image:logo.png[Alt] here.\n", 8)]
    #[case::line_start("= Doc A\n:imagesdir: img\n\nimage:logo.png[Alt] here.\n", 8)]
    #[case::on_macro_name("= Doc A\n:imagesdir: img\n\nText image:logo.png[Alt] here.\n", 6)]
    #[case::alone_on_line("= Doc A\n:imagesdir: img\n\nimage:logo.png[Alt]\n", 8)]
    #[case::list_item("= Doc A\n:imagesdir: img\n\n* image:logo.png[Alt]\n", 8)]
    #[case::section_title("= Doc A\n:imagesdir: img\n\n== A image:logo.png[Alt]\n", 12)]
    #[case::second_paragraph_line("= Doc A\n:imagesdir: img\n\nFirst.\nimage:logo.png[Alt]\n", 8)]
    #[case::default_table_cell(
        "= Doc A\n:imagesdir: img\n\n|===\n| image:logo.png[Alt]\n|===\n",
        10
    )]
    #[case::asciidoc_table_cell(
        "= Doc A\n:imagesdir: img\n\n|===\na| image:logo.png[Alt]\n|===\n",
        10
    )]
    #[case::second_cell_in_row(
        "= Doc A\n:imagesdir: img\n\n|===\n| A | image:logo.png[Alt]\n|===\n",
        14
    )]
    fn test_inline_image_hover_and_goto_use_imagesdir(#[case] doc: &str, #[case] character: u32) {
        let (_tmp, root) = canonical_tmp();
        let img = root.join("img");
        fs::create_dir(&img).unwrap();
        fs::write(img.join("logo.png"), "fake-png-data").unwrap();
        let a = root.join("a.adoc");
        fs::write(&a, doc).unwrap();
        let ws = Workspace::build(&root, &a);
        let macro_line = doc.lines().position(|l| l.contains("image:")).unwrap();
        let pos = Position {
            line: u32::try_from(macro_line).unwrap(),
            character,
        };

        let hover = resolve_hover(doc, pos, &a, Some(&ws)).unwrap();
        assert_eq!(hover, "image:img/logo.png[]\n\n\u{2713} File exists");
        let (target, line) = resolve_goto_definition(doc, pos, &a, Some(&ws)).unwrap();
        assert_eq!(target, img.join("logo.png"));
        assert_eq!(line, 1);
    }

    #[test]
    fn test_inline_image_missing_file() {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        let doc = "= Doc A\n\nText image:missing.png[] here.\n";
        fs::write(&a, doc).unwrap();
        let ws = Workspace::build(&root, &a);
        let pos = Position {
            line: 2,
            character: 12,
        };

        let hover = resolve_hover(doc, pos, &a, Some(&ws)).unwrap();
        assert_eq!(hover, "image:missing.png[]\n\n\u{2717} File not found");
        assert!(resolve_goto_definition(doc, pos, &a, Some(&ws)).is_none());
    }

    #[rstest]
    #[case::block(
        "= Doc\n:imagesdir: ../assets\n\nimage::https://example.com/r.svg[]\n",
        "image::",
        "https://example.com/r.svg"
    )]
    #[case::inline(
        "= Doc\n:imagesdir: ../assets\n\nimage:https://example.com/r.svg[] x\n",
        "image:",
        "https://example.com/r.svg"
    )]
    #[case::http(
        "= Doc\n\nx\n\nimage::http://example.com/r.svg[]\n",
        "image::",
        "http://example.com/r.svg"
    )]
    #[case::url_from_attribute(
        "= Doc\n:base: https://example.com\n\nimage::{base}/r.svg[]\n",
        "image::",
        "https://example.com/r.svg"
    )]
    #[case::url_imagesdir(
        "= Doc\n:imagesdir: https://example.com/img\n\nimage::r.svg[]\n",
        "image::",
        "https://example.com/img/r.svg"
    )]
    fn test_remote_image_is_not_a_local_file(
        #[case] doc: &str,
        #[case] macro_form: &str,
        #[case] url: &str,
    ) {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        fs::write(&a, doc).unwrap();
        let ws = Workspace::build(&root, &a);
        let macro_line = doc.lines().position(|l| l.starts_with("image")).unwrap();
        let pos = Position {
            line: u32::try_from(macro_line).unwrap(),
            character: 2,
        };

        assert_eq!(
            resolve_hover(doc, pos, &a, Some(&ws)).unwrap(),
            format!("{macro_form}{url}[]")
        );
        assert!(resolve_goto_definition(doc, pos, &a, Some(&ws)).is_none());

        let report = explain_position(doc, pos, &a, &a, Some(&ws));
        let res = &report["resolution"];
        assert_eq!(report["node"]["kind"], "image");
        assert_eq!(res["remote"], true);
        assert_eq!(res["display"], url);
        assert!(res["target"].is_null());
        assert!(res["canonical_target"].is_null());
        assert!(res["exists"].is_null());
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(&canonical_root, &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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

        let ws = Workspace::build(tmp.path(), &a);
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
        overrides.insert(a_canon.clone(), Arc::new(new_content.to_string()));

        let ws = Workspace::build_with_overrides(tmp.path(), &a, &overrides);

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
        overrides.insert(a_canon.clone(), Arc::new(new_a.to_string()));
        overrides.insert(b_canon.clone(), Arc::new(new_b.to_string()));

        let ws = Workspace::build_with_overrides(tmp.path(), &a, &overrides);

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
        overrides.insert(a_canon.clone(), Arc::new(new_content.to_string()));

        let ws = Workspace::build_with_overrides(tmp.path(), &a, &overrides);

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

    // --- adoc-0137: explain_position report ---

    /// Canonical temp dir, so raw and canonical document paths coincide.
    fn canonical_tmp() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        (tmp, root)
    }

    #[test]
    fn test_explain_attribute_ref_reports_layers_and_value() {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        fs::write(&a, "= Doc A\n:author: Alice\n\nWritten by {author}.\n").unwrap();
        let ws = Workspace::build(&root, &a);
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 3,
            character: 12,
        };

        let report = explain_position(&content, pos, &a, &a, Some(&ws));

        assert_eq!(report["node"]["kind"], "attribute_ref");
        assert_eq!(report["node"]["line"], 4);
        assert_eq!(report["node"]["fields"]["name"], "author");
        let res = &report["resolution"];
        assert_eq!(res["value"], "Alice");
        assert_eq!(res["winner"], "per_file");
        assert_eq!(res["per_file"]["registered"], true);
        assert_eq!(res["per_file"]["hit"]["value"], "Alice");
        assert_eq!(res["per_file"]["hit"]["source"]["line"], 2);
        assert_eq!(res["config"]["dir"], path_string(&root));
        assert!(res["builtin"].is_null());
    }

    #[test]
    fn test_explain_attribute_def_reports_resolution() {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        fs::write(&a, "= Doc A\n:author: Alice\n").unwrap();
        let ws = Workspace::build(&root, &a);
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 1,
            character: 3,
        };

        let report = explain_position(&content, pos, &a, &a, Some(&ws));

        assert_eq!(report["node"]["kind"], "attribute_def");
        assert_eq!(report["node"]["fields"]["name"], "author");
        assert_eq!(report["resolution"]["value"], "Alice");
        assert_eq!(report["resolution"]["winner"], "per_file");
    }

    #[test]
    fn test_explain_xref_reports_resolved_target() {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        let b = root.join("b.adoc");
        fs::write(
            &a,
            "= Doc A\n\nSee xref:b.adoc#my-anchor[link].\n\ninclude::b.adoc[]\n",
        )
        .unwrap();
        fs::write(&b, "[[my-anchor]]\n== Chapter B\n").unwrap();
        let ws = Workspace::build(&root, &a);
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 12,
        };

        let report = explain_position(&content, pos, &a, &a, Some(&ws));

        assert_eq!(report["node"]["kind"], "xref");
        assert_eq!(report["node"]["fields"]["target"], "b.adoc#my-anchor");
        assert_eq!(report["resolution"]["resolved"]["file"], path_string(&b));
        assert_eq!(report["resolution"]["resolved"]["line"], 1);
        assert!(report["resolution"]["error"].is_null());
    }

    #[test]
    fn test_explain_shorthand_xref_broken_reports_error() {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        fs::write(&a, "= Doc A\n\nSee <<nonexistent-anchor>>.\n").unwrap();
        let ws = Workspace::build(&root, &a);
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 8,
        };

        let report = explain_position(&content, pos, &a, &a, Some(&ws));

        assert_eq!(report["node"]["kind"], "shorthand_xref");
        assert!(report["resolution"]["resolved"].is_null());
        assert_eq!(
            report["resolution"]["error"],
            "Broken cross-reference: nonexistent-anchor"
        );
    }

    #[test]
    fn test_explain_include_reports_target_and_existence() {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        let b = root.join("sub.adoc");
        fs::write(&a, "= Doc A\n\ninclude::sub.adoc[]\n").unwrap();
        fs::write(&b, "== Sub\n").unwrap();
        let ws = Workspace::build(&root, &a);
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 12,
        };

        let report = explain_position(&content, pos, &a, &a, Some(&ws));

        assert_eq!(report["node"]["kind"], "include");
        assert_eq!(report["node"]["fields"]["path"], "sub.adoc");
        assert_eq!(report["resolution"]["target"], path_string(&b));
        assert_eq!(report["resolution"]["exists"], true);
        let (gd_target, _) = resolve_goto_definition(&content, pos, &a, Some(&ws)).unwrap();
        assert_eq!(
            report["resolution"]["canonical_target"],
            path_string(&gd_target)
        );
    }

    #[test]
    fn test_explain_include_missing_has_no_canonical_target() {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        fs::write(&a, "= Doc A\n\ninclude::missing.adoc[]\n").unwrap();
        let ws = Workspace::build(&root, &a);
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 12,
        };

        let report = explain_position(&content, pos, &a, &a, Some(&ws));

        assert_eq!(
            report["resolution"]["target"],
            path_string(&root.join("missing.adoc"))
        );
        assert_eq!(report["resolution"]["exists"], false);
        assert!(report["resolution"]["canonical_target"].is_null());
        assert!(resolve_goto_definition(&content, pos, &a, Some(&ws)).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn test_explain_include_through_symlink_reports_gd_canonical_target() {
        let (_tmp, root) = canonical_tmp();
        let real = root.join("real");
        fs::create_dir(&real).unwrap();
        let sub = real.join("sub.adoc");
        fs::write(&sub, "== Sub\n").unwrap();
        std::os::unix::fs::symlink(&real, root.join("link")).unwrap();
        let a = root.join("a.adoc");
        fs::write(&a, "= Doc A\n\ninclude::link/sub.adoc[]\n").unwrap();
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 12,
        };

        // Without a workspace the target is joined but not canonicalized,
        // so the symlinked raw target and gd's canonical path differ.
        let report = explain_position(&content, pos, &a, &a, None);

        let res = &report["resolution"];
        assert_eq!(
            res["target"],
            path_string(&root.join("link").join("sub.adoc"))
        );
        assert_eq!(res["canonical_target"], path_string(&sub));
        let (gd_target, _) = resolve_goto_definition(&content, pos, &a, None).unwrap();
        assert_eq!(res["canonical_target"], path_string(&gd_target));
    }

    #[test]
    fn test_explain_image_reports_target_display_and_existence() {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        fs::write(&a, "= Doc A\n:imagesdir: img\n\nimage::logo.png[]\n").unwrap();
        let ws = Workspace::build(&root, &a);
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 3,
            character: 9,
        };

        let report = explain_position(&content, pos, &a, &a, Some(&ws));

        assert_eq!(report["node"]["kind"], "image");
        assert_eq!(report["node"]["fields"]["path"], "logo.png");
        assert_eq!(
            report["resolution"]["target"],
            path_string(&root.join("img").join("logo.png"))
        );
        assert_eq!(report["resolution"]["display"], "img/logo.png");
        assert_eq!(report["resolution"]["exists"], false);
        assert_eq!(report["resolution"]["remote"], false);
        assert!(report["resolution"]["canonical_target"].is_null());
        assert!(resolve_goto_definition(&content, pos, &a, Some(&ws)).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn test_explain_image_through_symlink_reports_gd_canonical_target() {
        let (_tmp, root) = canonical_tmp();
        let real = root.join("real");
        fs::create_dir(&real).unwrap();
        let logo = real.join("logo.png");
        fs::write(&logo, "fake-png-data").unwrap();
        std::os::unix::fs::symlink(&real, root.join("link")).unwrap();
        let a = root.join("a.adoc");
        fs::write(&a, "= Doc A\n:imagesdir: link\n\nimage::logo.png[]\n").unwrap();
        let ws = Workspace::build(&root, &a);
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 3,
            character: 9,
        };

        let report = explain_position(&content, pos, &a, &a, Some(&ws));

        let res = &report["resolution"];
        assert_eq!(
            res["target"],
            path_string(&root.join("link").join("logo.png"))
        );
        assert_eq!(res["canonical_target"], path_string(&logo));
        let (gd_target, _) = resolve_goto_definition(&content, pos, &a, Some(&ws)).unwrap();
        assert_eq!(res["canonical_target"], path_string(&gd_target));
    }

    #[test]
    fn test_explain_anchor_reports_index_entry() {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        fs::write(&a, "= Doc A\n\nText with [[my-anchor]] inline.\n").unwrap();
        let ws = Workspace::build(&root, &a);
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 14,
        };

        let report = explain_position(&content, pos, &a, &a, Some(&ws));

        assert_eq!(report["node"]["kind"], "anchor");
        assert_eq!(report["node"]["fields"]["id"], "my-anchor");
        let def = &report["resolution"]["definition"];
        assert_eq!(def["file"], path_string(&a));
        assert_eq!(def["line"], 3);
        assert_eq!(def["kind"], "explicit");
    }

    #[test]
    fn test_explain_no_node_still_reports_request_and_attributes() {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        fs::write(&a, "= Doc A\n:author: Alice\n\nJust plain text.\n").unwrap();
        let ws = Workspace::build(&root, &a);
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 3,
            character: 5,
        };

        let report = explain_position(&content, pos, &a, &a, Some(&ws));

        assert!(report["node"].is_null());
        assert!(report["resolution"].is_null());
        let req = &report["request"];
        assert_eq!(req["raw_path"], path_string(&a));
        assert_eq!(req["canonical_path"], path_string(&a));
        assert_eq!(req["paths_differ"], false);
        assert_eq!(req["workspace_root"], path_string(&root));
        assert_eq!(req["in_documents_raw"], true);
        assert_eq!(req["in_documents_canonical"], true);
        let known = report["known_attributes"].as_array().unwrap();
        let author = known.iter().find(|k| k["name"] == "author").unwrap();
        assert_eq!(author["value"], "Alice");
        assert_eq!(author["layer"], "per_file");
        assert_eq!(author["source"]["line"], 2);
        let docdir = known.iter().find(|k| k["name"] == "docdir").unwrap();
        assert_eq!(docdir["layer"], "builtin");
    }

    #[test]
    fn test_explain_layer_names_are_stable_snake_case() {
        assert_eq!(layer_name(AttributeLayer::PerFile), "per_file");
        assert_eq!(layer_name(AttributeLayer::Config), "config");
        assert_eq!(layer_name(AttributeLayer::Builtin), "builtin");
    }

    #[test]
    fn test_explain_node_kind_names_are_stable_snake_case() {
        let s = String::new;
        assert_eq!(node_kind_name(&NodeKind::Include { path: s() }), "include");
        assert_eq!(node_kind_name(&NodeKind::Xref { target: s() }), "xref");
        assert_eq!(
            node_kind_name(&NodeKind::ShorthandXref { target: s() }),
            "shorthand_xref"
        );
        assert_eq!(
            node_kind_name(&NodeKind::AttributeRef { name: s() }),
            "attribute_ref"
        );
        assert_eq!(
            node_kind_name(&NodeKind::AttributeDef { name: s() }),
            "attribute_def"
        );
        assert_eq!(node_kind_name(&NodeKind::Anchor { id: s() }), "anchor");
        assert_eq!(
            node_kind_name(&NodeKind::Image {
                path: s(),
                inline: false
            }),
            "image"
        );
    }

    #[test]
    fn test_explain_anchor_kind_names_are_stable_snake_case() {
        assert_eq!(anchor_kind_name(AnchorKind::Explicit), "explicit");
        assert_eq!(
            anchor_kind_name(AnchorKind::ImplicitSection),
            "implicit_section"
        );
        assert_eq!(anchor_kind_name(AnchorKind::BlockId), "block_id");
    }

    #[test]
    fn test_explain_without_workspace() {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        fs::write(&a, "= Doc A\n\nWritten by {author}.\n").unwrap();
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 12,
        };

        let report = explain_position(&content, pos, &a, &a, None);

        assert!(report["request"]["workspace_root"].is_null());
        assert_eq!(report["request"]["in_documents_raw"], false);
        assert_eq!(report["request"]["in_documents_canonical"], false);
        assert_eq!(report["node"]["kind"], "attribute_ref");
        assert_eq!(report["resolution"]["error"], "no workspace");
        assert_eq!(report["known_attributes"], serde_json::json!([]));
    }

    #[cfg(unix)]
    #[test]
    fn test_explain_symlinked_path_exposes_raw_lookup_miss() {
        let (_tmp, root) = canonical_tmp();
        let real = root.join("real");
        fs::create_dir(&real).unwrap();
        let a = real.join("a.adoc");
        fs::write(&a, "= Doc A\n:author: Alice\n\nWritten by {author}.\n").unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let ws = Workspace::build(&real, &a);
        let raw = link.join("a.adoc");
        let content = fs::read_to_string(&raw).unwrap();
        let pos = Position {
            line: 3,
            character: 12,
        };

        let report = explain_position(&content, pos, &raw, &raw, Some(&ws));

        let req = &report["request"];
        assert_eq!(req["raw_path"], path_string(&raw));
        assert_eq!(req["canonical_path"], path_string(&a));
        assert_eq!(req["lookup_path"], path_string(&raw));
        assert_eq!(req["paths_differ"], true);
        assert_eq!(req["in_documents_raw"], false);
        assert_eq!(req["in_documents_canonical"], true);
        assert_eq!(report["resolution"]["per_file"]["registered"], false);
        assert!(report["resolution"]["value"].is_null());

        let fixed = explain_position(&content, pos, &raw, &a, Some(&ws));

        assert_eq!(fixed["request"]["raw_path"], path_string(&raw));
        assert_eq!(fixed["request"]["lookup_path"], path_string(&a));
        assert_eq!(fixed["resolution"]["per_file"]["registered"], true);
        assert_eq!(fixed["resolution"]["value"], "Alice");
    }
}
