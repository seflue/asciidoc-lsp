use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use tower_lsp_server::ls_types::Position;

use asciidoc_core::attributes::{config_attribute_at, AttributeSource};
use asciidoc_core::extractors::find_child_text;
use asciidoc_core::paths::{canonicalize_if_exists, join_doc_path, normalize_lexically};
use asciidoc_core::pdf_theme::{is_builtin_theme, is_pdf_path_attribute, PdfPath, PDF_THEME};
use asciidoc_core::types::{AnchorKind, DiagnosticKind, FileLocation, ResolvedXref};
use asciidoc_core::workspace::{
    AttributeDefinition, AttributeOrigin, AttributeScope, ConfigOverride, Workspace,
};

use crate::positions;
use crate::scopes::{
    collapse_if_agree, document_dir, file_display_name, path_scopes, scope_image_base, scope_label,
    unique_in_order, DocTarget,
};

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
    // a giant false passthrough node that swallows xref nodes).
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

/// Resolve a go-to-definition request to target files and lines
/// (1-indexed). An attribute reference yields every definition, the one
/// Asciidoctor uses first; an include, xref or image yields each scope's
/// existing target once (ADR-4).
///
/// Extracted from the `LanguageServer` handler for testability (V-8).
pub fn resolve_goto_definitions(
    content: &str,
    pos: Position,
    doc_path: &Path,
    workspace: Option<&Workspace>,
) -> Vec<(PathBuf, usize)> {
    let Some(node_info) = find_node_at_position(content, pos) else {
        return Vec::new();
    };
    match node_info.kind {
        NodeKind::AttributeRef { ref name } => {
            let Some(ws) = workspace else {
                return Vec::new();
            };
            ws.definition_sites(doc_path, node_info.line, name)
                .into_iter()
                .map(|source| (source.file, source.line))
                .collect()
        }
        NodeKind::Image { ref path, .. } => {
            // Each scope's file, each once.
            unique_in_order(
                resolve_image_targets(path, doc_path, workspace, node_info.line)
                    .into_iter()
                    .filter_map(|(_, target)| match target {
                        ImageTarget::Local { path: target, .. } => target.canonicalize().ok(),
                        ImageTarget::Remote { .. } => None,
                    })
                    .map(|canonical| (canonical, 1)),
            )
        }
        NodeKind::Include { ref path } => {
            let targets = resolve_doc_targets(path, doc_path, workspace, node_info.line);
            existing_targets(&targets)
                .into_iter()
                .map(|file| (file, 1))
                .collect()
        }
        NodeKind::Xref { ref target } | NodeKind::ShorthandXref { ref target } => {
            let Some(ws) = workspace else {
                return Vec::new();
            };
            unique_in_order(
                resolve_xref_targets(ws, doc_path, target, node_info.line)
                    .into_iter()
                    .filter_map(|(_, resolved)| resolved.ok())
                    .map(|resolved| (resolved.file, resolved.line)),
            )
        }
        // On a definition or anchor goto-definition is a no-op.
        NodeKind::AttributeDef { .. } | NodeKind::Anchor { .. } => Vec::new(),
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
                for source in ws.definition_sites(doc_path, node_info.line, name) {
                    r.push(FileLocation::new(source.file, source.line, 0, 0));
                }
            }
            r
        }
        NodeKind::Include { path } => {
            let targets = resolve_doc_targets(path, doc_path, Some(ws), node_info.line);
            let mut r = Vec::new();
            for target in existing_targets(&targets) {
                let filename = target.file_name().and_then(|f| f.to_str()).unwrap_or("");
                r.extend(ws.find_all_include_references(&target));
                r.extend(ws.find_all_file_xrefs(filename));
            }
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

/// Resolved PDF paths for hover: absolute paths as such, a bundled theme
/// marked as one, anything unresolvable as written; `pdf-fontsdir` entries
/// joined with `;`.
fn pdf_paths_display(name: &str, paths: &[PdfPath]) -> String {
    paths
        .iter()
        .map(|path| match path {
            PdfPath::Absolute(path) => path.display().to_string(),
            PdfPath::Builtin(theme) if name == PDF_THEME && is_builtin_theme(theme) => {
                format!("{theme} (built-in theme)")
            }
            PdfPath::Builtin(value) | PdfPath::Relative(value) | PdfPath::Unknown(value) => {
                value.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// Resolve a hover request to a formatted Markdown string.
///
/// Returns hover content for the node at the cursor position:
/// - `AttributeRef`: value + definition location; references in the value
///   replaced, a relative path from an `.asciidoctorconfig` also resolved
///   against the document's folder
/// - `AttributeDef`: value, with references replaced as for `AttributeRef`
/// - `pdf-theme`, `pdf-themesdir`, `pdf-fontsdir` (reference or definition):
///   resolved absolute path
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
        NodeKind::AttributeRef { name } if is_pdf_path_attribute(name) => {
            let resolution = workspace?.resolve_pdf_attribute(doc_path, name, None)?;
            let mut text = format!(":{name}: {}", pdf_paths_display(name, &resolution.paths));
            if let Some(source) = resolution.source {
                let display_path = file_display_name(&source.file);
                let _ = write!(text, "\n\nDefined in {display_path}:{}", source.line);
            }
            Some(text)
        }
        NodeKind::AttributeDef { name } if is_pdf_path_attribute(name) => {
            let resolution =
                workspace?.resolve_pdf_attribute(doc_path, name, Some(node_info.line))?;
            Some(format!(
                ":{name}: {}",
                pdf_paths_display(name, &resolution.paths)
            ))
        }
        NodeKind::AttributeRef { name } => {
            let ws = workspace?;
            let effective: Vec<(PathBuf, Option<AttributeDefinition>)> = ws
                .scopes_at(doc_path, node_info.line)
                .iter()
                .map(|scope| (scope.document().to_path_buf(), scope.definition(name)))
                .collect();
            if effective.iter().all(|(_, definition)| definition.is_none()) {
                return None;
            }
            let texts: Vec<(String, String)> = effective
                .into_iter()
                .map(|(document, definition)| {
                    let text = definition.map_or_else(
                        || "not set".to_string(),
                        |definition| {
                            let mut text = definition_hover(ws, doc_path, name, &definition);
                            if let Some(source) = &definition.source {
                                let display_path = file_display_name(&source.file);
                                let _ =
                                    write!(text, "\n\nDefined in {display_path}:{}", source.line);
                            }
                            text
                        },
                    );
                    (file_display_name(&document), text)
                })
                .collect();
            Some(per_document_hover(texts))
        }
        NodeKind::AttributeDef { name } => {
            let ws = workspace?;
            let file = ws.indexed_path(doc_path);
            let config_entry = ws
                .attributes()
                .is_config_file(&file)
                .then(|| config_attribute_at(&file, content, node_info.line, name))
                .flatten();
            let texts: Vec<(String, String)> = ws
                .scopes_at(doc_path, node_info.line)
                .iter()
                .filter_map(|scope| {
                    let text = if let Some(entry) = &config_entry {
                        config_definition_hover(ws, doc_path, scope, name, entry.raw.as_deref())
                    } else {
                        let definition = scope.definitions(name).find(|definition| {
                            definition.source.as_ref().is_some_and(|source| {
                                source.line == node_info.line && source.file == file
                            })
                        })?;
                        definition_hover(ws, doc_path, name, definition)
                    };
                    Some((scope_label(scope), text))
                })
                .collect();
            (!texts.is_empty()).then(|| per_document_hover(texts))
        }
        NodeKind::Xref { target } | NodeKind::ShorthandXref { target } => {
            let ws = workspace?;
            let resolved = resolve_xref_targets(ws, doc_path, target, node_info.line);
            if resolved.iter().all(|(_, resolved)| resolved.is_err()) {
                return None;
            }
            let texts: Vec<(String, String)> = resolved
                .into_iter()
                .map(|(label, resolved)| {
                    let text = resolved.map_or_else(
                        |_| "\u{2717} Target not found".to_string(),
                        |resolved| xref_hover(ws, target, &resolved),
                    );
                    (label, text)
                })
                .collect();
            Some(per_document_hover(texts))
        }
        NodeKind::Include { path } => {
            let mut targets = resolve_doc_targets(path, doc_path, workspace, node_info.line);
            // Scopes that reach the same file agree, however they spell it.
            collapse_if_agree(&mut targets, |a, b| a.path == b.path);
            let texts: Vec<(String, String)> = targets
                .into_iter()
                .map(|target| {
                    let mut text = format!("include::{}[]", target.substituted);
                    if target.path.exists() {
                        let section_count = workspace
                            .and_then(|ws| ws.documents().get(&target.path))
                            .map_or(0, |doc| doc.sections.len());
                        let _ = write!(text, "\n\n\u{2713} File exists | {section_count} sections");
                    } else {
                        text.push_str("\n\n\u{2717} File not found");
                    }
                    (target.label, text)
                })
                .collect();
            (!texts.is_empty()).then(|| per_document_hover(texts))
        }
        NodeKind::Image { path, inline } => {
            let macro_form = if *inline { "image:" } else { "image::" };
            let mut targets = resolve_image_targets(path, doc_path, workspace, node_info.line);
            // Scopes that reach the same file agree, however they spell it.
            collapse_if_agree(&mut targets, |a, b| a.1.same_place(&b.1));
            let texts: Vec<(String, String)> = targets
                .into_iter()
                .map(|(label, target)| {
                    let text = match target {
                        ImageTarget::Remote { url } => format!("{macro_form}{url}[]"),
                        ImageTarget::Local {
                            path: target,
                            display,
                        } => {
                            let status = if target.exists() {
                                "\u{2713} File exists"
                            } else {
                                "\u{2717} File not found"
                            };
                            format!("{macro_form}{display}[]\n\n{status}")
                        }
                    };
                    (label, text)
                })
                .collect();
            Some(per_document_hover(texts))
        }
        NodeKind::Anchor { .. } => None,
    }
}

/// Hover text for an xref that resolves: the section title, or the anchor
/// for an anchor outside a section title, then file and line.
fn xref_hover(ws: &Workspace, target: &str, resolved: &ResolvedXref) -> String {
    let section_title = ws.documents().get(&resolved.file).and_then(|doc| {
        doc.sections
            .iter()
            .find(|s| s.line == resolved.line)
            .map(|s| s.title.clone())
    });
    let display_path = file_display_name(&resolved.file);
    if let Some(title) = section_title {
        format!("\u{2192} {title}\n\n{display_path}:{}", resolved.line)
    } else {
        // Anchor without section (explicit anchor)
        let anchor_id = target.rsplit('#').next().unwrap_or(target);
        format!("[[{anchor_id}]]\n\n{display_path}:{}", resolved.line)
    }
}

/// Hover text from one text per scope, each with its label: the text alone
/// when all agree, else each one under its label (ADR-4).
fn per_document_hover(mut texts: Vec<(String, String)>) -> String {
    collapse_if_agree(&mut texts, |a, b| a.1 == b.1);
    if let [(_, text)] = texts.as_slice() {
        return text.clone();
    }
    texts
        .iter()
        .map(|(label, text)| format!("In {label}:\n{text}"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// An attribute entry as written: `:name: raw`, `:name:` for an empty value,
/// or `:name!:` for an unset.
fn entry_text(name: &str, raw: Option<&str>) -> String {
    match raw {
        None => format!(":{name}!:"),
        Some("") => format!(":{name}:"),
        Some(raw) => format!(":{name}: {raw}"),
    }
}

/// Hover text for one definition: the entry as written (`:name!:` for an
/// unset), then what it resolves to where it differs.
fn definition_hover(
    ws: &Workspace,
    doc_path: &Path,
    name: &str,
    definition: &AttributeDefinition,
) -> String {
    let (Some(raw), Some(value)) = (&definition.raw, &definition.value) else {
        return entry_text(name, None);
    };
    let mut text = entry_text(name, Some(raw));
    let resolved = if definition.origin == AttributeOrigin::Config {
        config_relative_path(ws, doc_path, value).map(|path| path.display().to_string())
    } else {
        None
    }
    .or_else(|| (value != raw).then(|| value.clone()));
    if let Some(resolved) = resolved {
        text.push_str("\n\nResolves to ");
        text.push_str(&resolved);
    }
    text
}

/// Hover text for the entry `:name: raw` (`None` for an unset) of the config
/// file `doc_path`: the line as written, then what it overrides: the entry loaded
/// before it in a config file, shown with its place relative to `doc_path`,
/// else a builtin. A config file sees the chain without itself, so `scope`
/// substitutes against what is above it.
fn config_definition_hover(
    ws: &Workspace,
    doc_path: &Path,
    scope: &AttributeScope<'_>,
    name: &str,
    raw: Option<&str>,
) -> String {
    let definition = AttributeDefinition {
        source: None,
        origin: AttributeOrigin::Config,
        raw: raw.map(str::to_string),
        value: raw.map(|raw| scope.substitute(raw)),
    };
    let mut text = definition_hover(ws, doc_path, name, &definition);
    let overridden = ws
        .config_override(doc_path, scope, name)
        .map(|overridden| match overridden {
            ConfigOverride::Entry { place, line, raw } => format!(
                "{} ({}:{line})",
                entry_text(name, raw.as_deref()),
                place.display()
            ),
            ConfigOverride::Builtin { raw } => entry_text(name, raw.as_deref()),
        });
    if let Some(overridden) = overridden {
        use std::fmt::Write;
        let _ = write!(text, "\n\nOverrides {overridden}");
    }
    text
}

/// A relative path value (`./`, `../`) from an `.asciidoctorconfig`, joined
/// onto the document directory of `doc_path`: the config value is prepended
/// text, so Asciidoctor resolves it where the document uses it.
fn config_relative_path(ws: &Workspace, doc_path: &Path, value: &str) -> Option<PathBuf> {
    if !(value.starts_with("./") || value.starts_with("../")) {
        return None;
    }
    Some(normalize_lexically(
        &ws.attributes().doc_dir(doc_path)?.join(value),
    ))
}

/// Where an image macro points after attribute substitution and `imagesdir`.
enum ImageTarget {
    /// A file: `path` is absolute, `display` is the human-readable path
    /// including imagesdir.
    Local { path: PathBuf, display: String },
    /// An `http(s)` URL, from the macro target or from `imagesdir`.
    Remote { url: String },
}

impl ImageTarget {
    /// Whether both point to the same file or URL.
    fn same_place(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Local { path: a, .. }, Self::Local { path: b, .. }) => a == b,
            (Self::Remote { url: a }, Self::Remote { url: b }) => a == b,
            _ => false,
        }
    }
}

fn is_remote_url(target: &str) -> bool {
    target.starts_with("http://") || target.starts_with("https://")
}

/// Where an image macro on `line` points, per top-level document and, for
/// a file others include, in the editor's preview of it (ADR-4): target,
/// `imagesdir` and `docdir` take the values in effect there. Each target
/// comes with its scope's label; without a workspace, one target with the
/// label of `doc_path`.
fn resolve_image_targets(
    image_path: &str,
    doc_path: &Path,
    workspace: Option<&Workspace>,
    line: usize,
) -> Vec<(String, ImageTarget)> {
    let file_dir = document_dir(doc_path, workspace);
    let Some(ws) = workspace else {
        let target = image_target(image_path, file_dir, "");
        return vec![(file_display_name(doc_path), target)];
    };
    path_scopes(ws, doc_path, line)
        .map(|scope| {
            let (base, imagesdir) = scope_image_base(&scope, file_dir);
            let target = image_target(&scope.substitute(image_path), &base, &imagesdir);
            (scope_label(&scope), target)
        })
        .collect()
}

/// An image target from its substituted path, the folder it resolves
/// against and the `imagesdir` shown in front of it.
fn image_target(resolved_path: &str, base: &Path, imagesdir: &str) -> ImageTarget {
    let display = if imagesdir.is_empty() || is_remote_url(resolved_path) {
        resolved_path.to_string()
    } else {
        format!("{imagesdir}/{resolved_path}")
    };
    if is_remote_url(&display) {
        return ImageTarget::Remote { url: display };
    }
    ImageTarget::Local {
        path: join_doc_path(base, resolved_path),
        display,
    }
}

/// Where an include or xref file path on `line` points, per top-level
/// document and, for a file others include, in the editor's preview of it
/// (ADR-4), without checking that it exists. Each target comes with its
/// scope's label and the path as substituted there; canonical when it
/// exists. Without a workspace, one target with the label of `doc_path`.
fn resolve_doc_targets(
    target_path: &str,
    doc_path: &Path,
    workspace: Option<&Workspace>,
    line: usize,
) -> Vec<DocTarget> {
    let dir = document_dir(doc_path, workspace);
    let Some(ws) = workspace else {
        return vec![DocTarget::new(
            file_display_name(doc_path),
            target_path,
            dir,
        )];
    };
    path_scopes(ws, doc_path, line)
        .map(|scope| DocTarget::new(scope_label(&scope), &scope.substitute(target_path), dir))
        .collect()
}

/// The distinct existing files among `targets`, in order.
fn existing_targets(targets: &[DocTarget]) -> Vec<PathBuf> {
    unique_in_order(
        targets
            .iter()
            .filter(|target| target.path.exists())
            .map(|target| target.path.clone()),
    )
}

/// How an xref on `line` resolves, per top-level document and, for a file
/// others include, in the editor's preview of it (ADR-4), each with its
/// scope's label. Shared by go-to-definition, hover and the explain report.
fn resolve_xref_targets(
    ws: &Workspace,
    doc_path: &Path,
    target: &str,
    line: usize,
) -> Vec<(String, Result<ResolvedXref, DiagnosticKind>)> {
    let xref = asciidoc_core::types::Xref {
        target: target.to_string(),
        display_text: None,
        line,
        span: None,
    };
    let dir = document_dir(doc_path, Some(ws));
    path_scopes(ws, doc_path, line)
        .map(|scope| {
            let resolved = ws
                .reference_index()
                .resolve_xref_with(&xref, ws.documents(), |file| {
                    Some(canonicalize_if_exists(join_doc_path(
                        dir,
                        &scope.substitute(file),
                    )))
                });
            (scope_label(&scope), resolved)
        })
        .collect()
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
    let line = pos.line as usize + 1;
    let known_attributes: Vec<Value> = workspace.map_or_default(|ws| {
        ws.scopes_at(lookup_path, line)
            .iter()
            .flat_map(|scope| {
                let label = scope_label(scope);
                scope.known().into_iter().map(move |(name, definition)| {
                    json!({
                        "scope": label,
                        "name": name,
                        "value": definition.value,
                        "origin": origin_name(definition.origin),
                        "source": source_json(definition.source.as_ref()),
                    })
                })
            })
            .collect()
    });

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
        NodeKind::AttributeRef { name } | NodeKind::AttributeDef { name } => workspace
            .map_or_else(no_workspace, |ws| {
                explain_attribute(ws, doc_path, name, info.line)
            }),
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
            let scopes: Vec<Value> = resolve_doc_targets(path, doc_path, workspace, info.line)
                .into_iter()
                .map(|target| {
                    // Same existence gate as go-to-definition: the path gd returns.
                    let exists = target.path.exists();
                    json!({
                        "scope": target.label,
                        "target": path_string(&target.joined),
                        "canonical_target": exists.then(|| path_string(&target.path)),
                        "exists": exists,
                    })
                })
                .collect();
            json!({ "scopes": scopes })
        }
        NodeKind::Image { path, .. } => {
            let scopes: Vec<Value> = resolve_image_targets(path, doc_path, workspace, info.line)
                .into_iter()
                .map(|(label, target)| explain_image(&label, target))
                .collect();
            json!({ "scopes": scopes })
        }
    }
}

fn explain_image(label: &str, target: ImageTarget) -> Value {
    match target {
        ImageTarget::Remote { url } => json!({
            "scope": label,
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
                "scope": label,
                "remote": false,
                "target": path_string(&target),
                "canonical_target": canonical_target.as_deref().map(path_string),
                "display": display,
                "exists": target.exists(),
            })
        }
    }
}

/// `name` on `line` per top-level document (ADR-4: no preview scope for
/// attribute values).
fn explain_attribute(ws: &Workspace, doc_path: &Path, name: &str, line: usize) -> Value {
    let scopes: Vec<Value> = ws
        .scopes_at(doc_path, line)
        .iter()
        .map(|scope| {
            json!({
                "scope": scope_label(scope),
                "value": scope.value(name),
                "definition": scope.definition(name).as_ref().map(definition_json),
            })
        })
        .collect();
    json!({ "name": name, "scopes": scopes })
}

fn definition_json(definition: &AttributeDefinition) -> Value {
    json!({
        "origin": origin_name(definition.origin),
        "raw": definition.raw,
        "value": definition.value,
        "source": source_json(definition.source.as_ref()),
    })
}

fn explain_xref(ws: &Workspace, doc_path: &Path, target: &str, line: usize) -> Value {
    let scopes: Vec<Value> = resolve_xref_targets(ws, doc_path, target, line)
        .into_iter()
        .map(|(label, resolved)| match resolved {
            Ok(resolved) => json!({
                "scope": label,
                "resolved": { "file": path_string(&resolved.file), "line": resolved.line },
                "error": null,
            }),
            Err(e) => json!({ "scope": label, "resolved": null, "error": e.to_string() }),
        })
        .collect();
    json!({ "scopes": scopes })
}

fn source_json(source: Option<&AttributeSource>) -> Value {
    source.map_or(
        Value::Null,
        |s| json!({ "file": path_string(&s.file), "line": s.line }),
    )
}

/// Stable report name of an attribute origin, independent of Rust's `Debug`.
fn origin_name(origin: AttributeOrigin) -> &'static str {
    match origin {
        AttributeOrigin::Document => "document",
        AttributeOrigin::Config => "config",
        AttributeOrigin::Builtin => "builtin",
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

// ---------------------------------------------------------------------------
// Internal classification helpers
// ---------------------------------------------------------------------------

/// Check if the node at the given point is inside a `block_macro` with name
/// "include" or "image", or inside a block anchor.
fn classify_block_node(
    root: tree_sitter::Node,
    point: tree_sitter::Point,
    source: &[u8],
) -> Option<NodeInfo> {
    let node = root.descendant_for_point_range(point, point)?;

    let mut current = Some(node);
    while let Some(n) = current {
        // [[anchor-id]] on a line of its own, above the block it names
        if n.kind() == "block_anchor" {
            if let Some(id) = find_child_text(n, "id", source) {
                return Some(NodeInfo {
                    kind: NodeKind::Anchor { id },
                    line: n.start_position().row + 1,
                });
            }
        }
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
// Rename support
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

            // Attribute definitions: :name: → find columns via text search
            for source in ws.definition_sites(doc_path, node_info.line, name) {
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
// Column-finder helpers
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
    use crate::test_support::{opened, scanned};
    use rstest::rstest;
    use std::fs;

    // --- find_node_at_position tests ---

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

    // --- resolve_goto_definitions for include ---

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

        assert_eq!(
            resolve_goto_definitions(&content, pos, &a, None),
            vec![(b.canonicalize().unwrap(), 1)]
        );
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

        assert!(resolve_goto_definitions(&content, pos, &a, None).is_empty());
    }

    // --- resolve_goto_definitions for xref ---

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

        assert_eq!(
            resolve_goto_definitions(&content, pos, &a, Some(&ws)),
            vec![(b.canonicalize().unwrap(), 1)]
        );
    }

    // xrefs in default (`|`) and AsciiDoc (`a|`) table cells.
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

        assert_eq!(
            resolve_goto_definitions(&content, pos, &a, Some(&ws)),
            vec![(b, 1)]
        );
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

        assert_eq!(
            resolve_goto_definitions(&content, pos, &a, Some(&ws)),
            vec![(b.canonicalize().unwrap(), 1)]
        );
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

        assert!(resolve_goto_definitions(&content, pos, &a, Some(&ws)).is_empty());
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

        assert!(resolve_goto_definitions(&content, pos, &a, Some(&ws)).is_empty());
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

        assert_eq!(
            resolve_goto_definitions(&content, pos, &a, Some(&ws)),
            vec![(b.canonicalize().unwrap(), 1)]
        );
    }

    // --- resolve_goto_definitions for attribute references ---

    #[test]
    fn test_goto_def_attribute_doc_header() {
        let content = "= Doc A\n:author: Alice\n\nWritten by {author}.\n";
        let (_tmp, ws, paths) = scanned(&[("a.adoc", content)]);
        // character 12 = 'a' in "{author}" on line 3 (0-indexed)
        let pos = Position {
            line: 3,
            character: 12,
        };

        // :author: is on line 2 (1-indexed)
        assert_eq!(
            resolve_goto_definitions(content, pos, &paths[0], Some(&ws)),
            vec![(paths[0].clone(), 2)]
        );
    }

    #[test]
    fn test_goto_def_attribute_config() {
        let content = "= Doc A\n\nImages in {imagesdir} folder.\n";
        let (_tmp, ws, paths) = scanned(&[
            (".asciidoctorconfig", ":imagesdir: images\n"),
            ("a.adoc", content),
        ]);
        // character 11 = 'i' in "{imagesdir}" on line 2 (0-indexed)
        let pos = Position {
            line: 2,
            character: 11,
        };

        assert_eq!(
            resolve_goto_definitions(content, pos, &paths[1], Some(&ws)),
            vec![(paths[0].clone(), 1)]
        );
    }

    #[test]
    fn test_goto_def_attribute_defined_in_included_file() {
        let content = "= Main\ninclude::attrs.adoc[]\n\nSee {custom}.\n";
        let (_tmp, ws, paths) =
            scanned(&[("attrs.adoc", ":custom: value\n"), ("main.adoc", content)]);
        let pos = Position {
            line: 3,
            character: 6,
        };

        assert_eq!(
            resolve_goto_definitions(content, pos, &paths[1], Some(&ws)),
            vec![(paths[0].clone(), 1)]
        );
    }

    /// The attribute, its definition, the include and the rename and
    /// references that reach the definition, with the definition in a file
    /// included next to the chapter that uses it.
    #[test]
    fn test_attribute_defined_in_sibling_include_is_reachable() {
        let chapter = "== Chapter\n\nSee {custom}.\n";
        let (_tmp, ws, paths) = scanned(&[
            (
                "main.adoc",
                "= Main\ninclude::attrs.adoc[]\ninclude::chapter.adoc[]\n",
            ),
            ("attrs.adoc", ":custom: some value\n"),
            ("chapter.adoc", chapter),
        ]);
        let (attrs, chapter_path) = (&paths[1], &paths[2]);
        let pos = at_reference(2);

        assert_eq!(
            resolve_goto_definitions(chapter, pos, chapter_path, Some(&ws)),
            vec![(attrs.clone(), 1)]
        );
        assert_eq!(
            resolve_hover(chapter, pos, chapter_path, Some(&ws)).unwrap(),
            ":custom: some value\n\nDefined in attrs.adoc:1"
        );
        let edits = resolve_rename(chapter, pos, chapter_path, Some(&ws), &HashMap::new()).unwrap();
        assert!(edits.contains_key(attrs), "edits: {edits:?}");
        let refs = resolve_references(chapter, pos, chapter_path, Some(&ws), true).unwrap();
        assert!(
            refs.iter().any(|loc| loc.path == *attrs && loc.line == 1),
            "refs: {refs:?}"
        );
    }

    #[test]
    fn test_goto_def_attribute_used_before_defining_include() {
        // Asciidoctor leaves {custom} unresolved here; navigation still finds it.
        let content = "= Main\n\nSee {custom}.\n\ninclude::attrs.adoc[]\n";
        let (_tmp, ws, paths) =
            scanned(&[("attrs.adoc", ":custom: value\n"), ("main.adoc", content)]);
        let pos = Position {
            line: 2,
            character: 6,
        };

        assert_eq!(
            resolve_goto_definitions(content, pos, &paths[1], Some(&ws)),
            vec![(paths[0].clone(), 1)]
        );
    }

    #[test]
    fn test_goto_def_attribute_takes_last_definition_before_reference() {
        let content = "= Main\ninclude::attrs.adoc[]\n\nSee {custom}.\n\n:custom: second\n";
        let (_tmp, ws, paths) =
            scanned(&[("attrs.adoc", ":custom: first\n"), ("main.adoc", content)]);
        let pos = Position {
            line: 3,
            character: 6,
        };

        // Effective definition first, then the later one in reading order.
        assert_eq!(
            resolve_goto_definitions(content, pos, &paths[1], Some(&ws)),
            vec![(paths[0].clone(), 1), (paths[1].clone(), 6)]
        );
    }

    #[test]
    fn test_goto_def_attribute_undefined() {
        let content = "= Doc A\n\nSee {nonexistent} here.\n";
        let (_tmp, ws, paths) = scanned(&[("a.adoc", content)]);
        let pos = Position {
            line: 2,
            character: 5,
        };

        assert!(resolve_goto_definitions(content, pos, &paths[0], Some(&ws)).is_empty());
    }

    // --- find_node_at_position for {attribute} references ---

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

    // --- xref after list items ---

    #[test]
    fn test_find_node_xref_after_list_items() {
        // Regression: xref in paragraph after unordered list is not found.
        // This matches the pattern in 09_architecture_decisions.adoc:46.
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
        assert!(info.is_some(), "xref after list items must be found");
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
        // xref in 09_architecture_decisions.adoc line 46 was not found
        // because `+` in "(block + inline)" created a false passthrough spanning
        // spanning most of the document, swallowing the xref nodes.
        let content = fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/chapters/09_architecture_decisions.adoc"
        ))
        .unwrap();

        // Line 46 (1-indexed): "...see xref:05_building_block_view.adoc#building-blocks[...]."
        let pos = Position {
            line: 45,
            character: 50,
        };
        let info = find_node_at_position(&content, pos);
        assert!(info.is_some(), "xref on line 46 must be found");
        assert_eq!(
            info.unwrap().kind,
            NodeKind::Xref {
                target: "05_building_block_view.adoc#building-blocks".into()
            }
        );
    }

    #[test]
    fn test_find_node_xref_real_adr_second_passthrough_xref() {
        // second affected xref on line 68, also inside the false passthrough.
        let content = fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/chapters/09_architecture_decisions.adoc"
        ))
        .unwrap();

        // Line 68 (1-indexed): "...documented in xref:01_introduction_and_goals.adoc[...]..."
        // "xref:" starts at character 91, target at 96
        let pos = Position {
            line: 67,
            character: 100,
        };
        let info = find_node_at_position(&content, pos);
        assert!(info.is_some(), "xref on line 68 must be found");
        assert_eq!(
            info.unwrap().kind,
            NodeKind::Xref {
                target: "01_introduction_and_goals.adoc".into()
            }
        );
    }

    // --- find_node_at_position for attribute definitions ---

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

    // --- resolve_references for attribute references ---

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

    /// Goto and references from inside an indexed `.asciidoctorconfig.adoc`
    /// list its own declaration once (committed fixture
    /// `tests/fixtures/config_both_files`).
    #[test]
    fn test_references_in_config_file_list_declaration_once() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/config_both_files")
            .canonicalize()
            .unwrap();
        let config = root.join(".asciidoctorconfig.adoc");
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        let content = fs::read_to_string(&config).unwrap();
        // cursor on {adoc-only} in `:derived: {adoc-only}/x`
        let pos = Position {
            line: 2,
            character: 12,
        };

        assert_eq!(
            resolve_goto_definitions(&content, pos, &config, Some(&ws)),
            vec![(config.clone(), 2)]
        );
        let refs = resolve_references(&content, pos, &config, Some(&ws), true).unwrap();
        let found: Vec<(&Path, usize)> = refs
            .iter()
            .map(|loc| (loc.path.as_path(), loc.line))
            .collect();
        let doc = root.join("doc.adoc");
        assert_eq!(
            found,
            vec![
                (config.as_path(), 2),
                (config.as_path(), 3),
                (doc.as_path(), 3)
            ]
        );
    }

    // --- Integration test — gr on {attr} in .adoc ---

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

    // --- Integration test — gr on definition in .asciidoctorconfig ---

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

    // --- find_node_at_position for anchors ---

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

    // --- resolve_references for includes ---

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

    // --- resolve_references for anchors ---

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

    // --- resolve_references for xrefs ---

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

    // --- Integration test — gr on include (multi-file) ---

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

    // --- Integration test — gr on anchor (multi-file) ---

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

    // --- find_node_at_position for image block macro ---

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

    // --- resolve_goto_definitions for image ---

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

        assert_eq!(
            resolve_goto_definitions(&content, pos, &a, None),
            vec![(img.canonicalize().unwrap(), 1)]
        );
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

        assert_eq!(
            resolve_goto_definitions(&content, pos, &a, Some(&ws)),
            vec![(img.canonicalize().unwrap(), 1)]
        );
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

        assert_eq!(
            resolve_goto_definitions(&content, pos, &a, Some(&ws)),
            vec![(img.canonicalize().unwrap(), 1)]
        );
    }

    // --- resolve_hover tests ---

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

    // --- hover substitutes references in attribute values ---

    #[test]
    fn test_hover_attribute_ref_substitutes_references() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let content =
            "= Doc A\n:assets: doc/assets\n:imagesdir: {assets}/img\n\nSee {imagesdir}.\n";
        fs::write(&a, content).unwrap();

        let ws = Workspace::build(tmp.path(), &a);
        let pos = Position {
            line: 4,
            character: 6,
        };

        let hover = resolve_hover(content, pos, &a, Some(&ws)).unwrap();
        assert!(
            hover.starts_with(":imagesdir: {assets}/img\n\nResolves to doc/assets/img"),
            "got: {hover}"
        );
    }

    /// `See {name}.` puts the reference at character 5.
    fn at_reference(line: u32) -> Position {
        Position { line, character: 5 }
    }

    #[test]
    fn test_hover_attribute_substitutes_previous_value_of_itself() {
        let content = "= M\n:imagesdir: img\n:imagesdir: {imagesdir}/sub\n\nSee {imagesdir}.\n";
        let (_tmp, ws, paths) = scanned(&[("m.adoc", content)]);
        let hover = resolve_hover(content, at_reference(4), &paths[0], Some(&ws)).unwrap();
        assert_eq!(
            hover,
            ":imagesdir: {imagesdir}/sub\n\nResolves to img/sub\n\nDefined in m.adoc:3"
        );
    }

    #[test]
    fn test_hover_attribute_substitutes_values_from_other_files() {
        // `{base}` is read in attrs.adoc, before chapter.adoc changes it.
        let chapter = "== C\n\nSee {custom}.\n:base: LATER\n";
        let (_tmp, ws, paths) = scanned(&[
            (
                "main.adoc",
                "= M\n:base: B\ninclude::attrs.adoc[]\ninclude::chapter.adoc[]\n",
            ),
            ("attrs.adoc", ":custom: {base}/x\n"),
            ("chapter.adoc", chapter),
        ]);
        let hover = resolve_hover(chapter, at_reference(2), &paths[2], Some(&ws)).unwrap();
        assert_eq!(
            hover,
            ":custom: {base}/x\n\nResolves to B/x\n\nDefined in attrs.adoc:1"
        );
    }

    #[test]
    fn test_hover_attribute_def_shows_its_own_value() {
        let content = "= M\n:x: one\n:x: {x} two\n";
        let (_tmp, ws, paths) = scanned(&[("m.adoc", content)]);
        let first = Position {
            line: 1,
            character: 1,
        };
        let second = Position {
            line: 2,
            character: 1,
        };
        assert_eq!(
            resolve_hover(content, first, &paths[0], Some(&ws)).unwrap(),
            ":x: one"
        );
        assert_eq!(
            resolve_hover(content, second, &paths[0], Some(&ws)).unwrap(),
            ":x: {x} two\n\nResolves to one two"
        );
    }

    #[test]
    fn test_hover_attribute_def_in_shared_file_shows_each_master() {
        let shared = ":custom: {base}/x\n";
        let (_tmp, ws, paths) = scanned(&[
            ("m1.adoc", "= M1\n:base: A\ninclude::shared.adoc[]\n"),
            ("m2.adoc", "= M2\n:base: B\ninclude::shared.adoc[]\n"),
            ("shared.adoc", shared),
        ]);
        let pos = Position {
            line: 0,
            character: 1,
        };
        assert_eq!(
            resolve_hover(shared, pos, &paths[2], Some(&ws)).unwrap(),
            "In m1.adoc:\n:custom: {base}/x\n\nResolves to A/x\n\n\
             In m2.adoc:\n:custom: {base}/x\n\nResolves to B/x"
        );
    }

    /// A definition hover shows the line under the cursor. In a config file
    /// the config loaded before it only adds what the line overrides, with
    /// the place of that entry relative to the hovered file. Files are open
    /// as in the editor, so the config files are documents of the workspace.
    #[test]
    fn test_hover_attribute_def_in_config_file_shows_its_own_line_and_what_it_overrides() {
        let child = ":d: down\n";
        let (_tmp, ws, paths) = opened(&[
            (".asciidoctorconfig", ":d: ./up\n"),
            ("sub/.asciidoctorconfig", child),
            ("sub/b.adoc", "= B\n"),
        ]);
        let pos = Position {
            line: 0,
            character: 1,
        };
        assert_eq!(
            resolve_hover(child, pos, &paths[1], Some(&ws)).unwrap(),
            ":d: down\n\nOverrides :d: ./up (../.asciidoctorconfig:1)"
        );
    }

    /// The line's own value is substituted against the chain above, so a
    /// reference to the overridden name reads the parent's value.
    #[test]
    fn test_hover_attribute_def_in_config_file_substitutes_against_the_config_above() {
        let child = ":d: {d}/more\n";
        let (_tmp, ws, paths) = opened(&[
            (".asciidoctorconfig", ":other: o\n:d: ./up\n"),
            ("sub/.asciidoctorconfig", child),
            ("sub/b.adoc", "= B\n"),
        ]);
        let pos = Position {
            line: 0,
            character: 1,
        };
        let sub = paths[1].parent().unwrap();
        assert_eq!(
            resolve_hover(child, pos, &paths[1], Some(&ws)).unwrap(),
            format!(
                ":d: {{d}}/more\n\nResolves to {}/up/more\n\nOverrides :d: ./up (../.asciidoctorconfig:2)",
                sub.display()
            )
        );
    }

    /// Both config files of one folder load, the one without extension last:
    /// it overrides its sibling.
    #[test]
    fn test_hover_attribute_def_in_config_file_overrides_its_sibling_config() {
        let plain = ":d: plain\n";
        let (_tmp, ws, paths) = opened(&[
            (".asciidoctorconfig.adoc", ":d: adoc\n"),
            (".asciidoctorconfig", plain),
            ("a.adoc", "= A\n"),
        ]);
        let pos = Position {
            line: 0,
            character: 1,
        };
        assert_eq!(
            resolve_hover(plain, pos, &paths[1], Some(&ws)).unwrap(),
            ":d: plain\n\nOverrides :d: adoc (.asciidoctorconfig.adoc:1)"
        );
    }

    /// The sibling loaded after a config file does not count as overridden
    /// by it: the `.adoc` file loads first, so nothing is above its line.
    #[test]
    fn test_hover_attribute_def_in_adoc_config_file_does_not_override_its_plain_sibling() {
        let adoc = ":d: adoc\n";
        let (_tmp, ws, paths) = opened(&[
            (".asciidoctorconfig.adoc", adoc),
            (".asciidoctorconfig", ":d: plain\n"),
            ("a.adoc", "= A\n"),
        ]);
        let pos = Position {
            line: 0,
            character: 1,
        };
        assert_eq!(
            resolve_hover(adoc, pos, &paths[0], Some(&ws)).unwrap(),
            ":d: adoc"
        );
    }

    /// A definition in the `.adoc` config file shows its own line too, not
    /// the definition of a config above.
    #[test]
    fn test_hover_attribute_def_in_adoc_config_file_shows_its_own_line() {
        let child = ":d: down\n";
        let (_tmp, ws, paths) = opened(&[
            (".asciidoctorconfig", ":d: up\n"),
            ("sub/.asciidoctorconfig.adoc", child),
            ("sub/b.adoc", "= B\n"),
        ]);
        let pos = Position {
            line: 0,
            character: 1,
        };
        assert_eq!(
            resolve_hover(child, pos, &paths[1], Some(&ws)).unwrap(),
            ":d: down\n\nOverrides :d: up (../.asciidoctorconfig:1)"
        );
    }

    /// No config above defines the name: the entry overrides nothing, not
    /// itself.
    #[test]
    fn test_hover_attribute_def_in_config_file_does_not_override_itself() {
        let child = ":kept: k\n:local: z\n";
        let (_tmp, ws, paths) = opened(&[
            (".asciidoctorconfig", ":kept: k\n"),
            ("sub/.asciidoctorconfig", child),
            ("sub/b.adoc", "= B\n"),
        ]);
        let pos = Position {
            line: 1,
            character: 1,
        };
        assert_eq!(
            resolve_hover(child, pos, &paths[1], Some(&ws)).unwrap(),
            ":local: z"
        );
    }

    /// The entry overridden is the last one before the file in load order,
    /// from whichever folder above, and an unset counts.
    #[test]
    fn test_hover_attribute_def_in_config_file_overrides_the_nearest_config_above() {
        let leaf = ":a: leaf\n:b: leaf\n";
        let (_tmp, ws, paths) = opened(&[
            (".asciidoctorconfig", ":a: root\n:b: root\n"),
            ("mid/.asciidoctorconfig", ":b!:\n"),
            ("mid/leaf/.asciidoctorconfig", leaf),
            ("mid/leaf/c.adoc", "= C\n"),
        ]);
        let hover = |line| {
            let pos = Position { line, character: 1 };
            resolve_hover(leaf, pos, &paths[2], Some(&ws)).unwrap()
        };
        assert_eq!(
            hover(0),
            ":a: leaf\n\nOverrides :a: root (../../.asciidoctorconfig:1)"
        );
        assert_eq!(
            hover(1),
            ":b: leaf\n\nOverrides :b!: (../.asciidoctorconfig:1)"
        );
    }

    /// A value continued with `\` shows complete, the lines joined.
    #[test]
    fn test_hover_attribute_def_in_config_file_shows_a_continued_value_complete() {
        let config = ":long: one \\\n  two \\\nthree\n:after: next\n";
        let (_tmp, ws, paths) = opened(&[(".asciidoctorconfig", config), ("a.adoc", "= A\n")]);
        let pos = Position {
            line: 0,
            character: 1,
        };
        assert_eq!(
            resolve_hover(config, pos, &paths[0], Some(&ws)).unwrap(),
            ":long: one two three"
        );
    }

    /// A config file in the workspace root, open in the editor, shows its
    /// line even though no document sits next to it.
    #[test]
    fn test_hover_attribute_def_in_opened_root_config_file_shows_its_own_line() {
        let config = ":d: only\n";
        let (_tmp, ws, paths) = opened(&[(".asciidoctorconfig", config)]);
        let pos = Position {
            line: 0,
            character: 1,
        };
        assert_eq!(
            resolve_hover(config, pos, &paths[0], Some(&ws)).unwrap(),
            ":d: only"
        );
    }

    /// A builtin is no config file: the override has no place to show.
    #[test]
    fn test_hover_attribute_def_in_config_file_overriding_a_builtin_names_no_place() {
        let config = ":env-asciidoc-lsp: x\n";
        let (_tmp, ws, paths) = opened(&[(".asciidoctorconfig", config), ("a.adoc", "= A\n")]);
        let pos = Position {
            line: 0,
            character: 1,
        };
        assert_eq!(
            resolve_hover(config, pos, &paths[0], Some(&ws)).unwrap(),
            ":env-asciidoc-lsp: x\n\nOverrides :env-asciidoc-lsp:"
        );
    }

    /// Asciidoctor prepends the config as text, so an earlier line of the same
    /// file counts for a later reference; only the file's later lines do not.
    #[test]
    fn test_hover_attribute_def_in_config_file_reads_its_earlier_lines() {
        let child = ":a: z\n:c: {a}\n";
        let (_tmp, ws, paths) = opened(&[
            (".asciidoctorconfig", ":a: x\n"),
            ("sub/.asciidoctorconfig", child),
            ("sub/b.adoc", "= B\n"),
        ]);
        let pos = Position {
            line: 1,
            character: 1,
        };
        assert_eq!(
            resolve_hover(child, pos, &paths[1], Some(&ws)).unwrap(),
            ":c: {a}\n\nResolves to z"
        );
    }

    /// A path with `..` is not the key the workspace indexes: the cursor line
    /// must still be matched to the document's own definition.
    #[test]
    fn test_hover_attribute_def_with_non_canonical_path() {
        let content = "= M\n:x: one\n:x: {x} two\n";
        let (_tmp, ws, paths) = scanned(&[("m.adoc", content), ("sub/b.adoc", "= B\n")]);
        let non_canonical = paths[0]
            .parent()
            .unwrap()
            .join("sub")
            .join("..")
            .join("m.adoc");
        let pos = Position {
            line: 2,
            character: 1,
        };
        assert_eq!(
            resolve_hover(content, pos, &non_canonical, Some(&ws)).unwrap(),
            ":x: {x} two\n\nResolves to one two"
        );
    }

    #[test]
    fn test_hover_attribute_def_in_config_file_without_config_above_shows_its_own_line() {
        let config = ":d: only\n";
        let (_tmp, ws, paths) = scanned(&[(".asciidoctorconfig", config), ("a.adoc", "= A\n")]);
        let pos = Position {
            line: 0,
            character: 1,
        };
        assert_eq!(
            resolve_hover(config, pos, &paths[0], Some(&ws)).unwrap(),
            ":d: only"
        );
    }

    #[test]
    fn test_hover_attribute_def_in_file_outside_workspace_is_none() {
        let (_tmp, ws, _paths) = scanned(&[("a.adoc", "= A\n")]);
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("o.adoc");
        let content = "= O\n:y: v\n";
        fs::write(&file, content).unwrap();
        let pos = Position {
            line: 1,
            character: 1,
        };
        assert!(resolve_hover(content, pos, &file, Some(&ws)).is_none());
    }

    #[test]
    fn test_hover_attribute_same_line_reference_means_previous_value() {
        let content = "= M\n:a: one\n:a: {a} two\n\nSee {a}.\n";
        let (_tmp, ws, paths) = scanned(&[("m.adoc", content)]);
        let pos = Position {
            line: 2,
            character: 5,
        };
        let hover = resolve_hover(content, pos, &paths[0], Some(&ws)).unwrap();
        assert_eq!(hover, ":a: one\n\nDefined in m.adoc:2");
    }

    #[test]
    fn test_hover_attribute_unset_before_reference() {
        let content = "= M\n:x: one\n:x!:\n\nSee {x}.\n";
        let (_tmp, ws, paths) = scanned(&[("m.adoc", content)]);
        let hover = resolve_hover(content, at_reference(4), &paths[0], Some(&ws)).unwrap();
        assert_eq!(hover, ":x!:\n\nDefined in m.adoc:3");
    }

    #[test]
    fn test_hover_attribute_document_value_is_not_a_config_path() {
        let chapter = "== C\n\nSee {d}.\n";
        let (_tmp, ws, paths) = scanned(&[
            (".asciidoctorconfig", ":d: ./cfgdir\n"),
            (
                "main.adoc",
                "= M\ninclude::attrs.adoc[]\ninclude::sub/ch.adoc[]\n",
            ),
            ("attrs.adoc", ":d: ../docdir\n"),
            ("sub/ch.adoc", chapter),
        ]);
        let hover = resolve_hover(chapter, at_reference(2), &paths[3], Some(&ws)).unwrap();
        assert_eq!(hover, ":d: ../docdir\n\nDefined in attrs.adoc:1");
    }

    #[test]
    fn test_goto_def_attribute_lists_effective_definition_first() {
        // Last before the reference wins, then the rest in reading order.
        let content = "= M\n:x: one\n:x: two\n\nSee {x}.\n\n:x: three\n";
        let (_tmp, ws, paths) = scanned(&[("m.adoc", content)]);
        let m = &paths[0];
        let targets = resolve_goto_definitions(content, at_reference(4), m, Some(&ws));
        assert_eq!(
            targets,
            vec![(m.clone(), 3), (m.clone(), 2), (m.clone(), 7)]
        );
    }

    #[test]
    fn test_goto_def_attribute_config_comes_before_later_definition() {
        let content = "= M\n\nSee {x}.\n\n:x: doc\n";
        let (_tmp, ws, paths) =
            scanned(&[(".asciidoctorconfig", ":x: cfg\n"), ("m.adoc", content)]);
        let m = &paths[1];
        let targets = resolve_goto_definitions(content, at_reference(2), m, Some(&ws));
        assert_eq!(targets, vec![(paths[0].clone(), 1), (m.clone(), 5)]);
        let hover = resolve_hover(content, at_reference(2), m, Some(&ws)).unwrap();
        assert!(hover.starts_with(":x: cfg\n"), "got: {hover}");
    }

    #[test]
    fn test_goto_def_attribute_in_second_top_level_document() {
        let chapter = "== C\n\nSee {x}.\n";
        let (_tmp, ws, paths) = scanned(&[
            ("a-draft.adoc", "= A\ninclude::ch.adoc[]\n"),
            ("b-book.adoc", "= B\n:x: book\ninclude::ch.adoc[]\n"),
            ("ch.adoc", chapter),
        ]);
        let targets = resolve_goto_definitions(chapter, at_reference(2), &paths[2], Some(&ws));
        assert_eq!(targets, vec![(paths[1].clone(), 2)]);
    }

    #[test]
    fn test_goto_def_attribute_offers_each_top_level_document() {
        let chapter = "== C\n\nSee {x}.\n";
        let (_tmp, ws, paths) = scanned(&[
            ("a-draft.adoc", "= A\n:x: draft\ninclude::ch.adoc[]\n"),
            ("b-book.adoc", "= B\n:x: book\ninclude::ch.adoc[]\n"),
            ("ch.adoc", chapter),
        ]);
        let targets = resolve_goto_definitions(chapter, at_reference(2), &paths[2], Some(&ws));
        assert_eq!(targets, vec![(paths[0].clone(), 2), (paths[1].clone(), 2)]);
    }

    #[test]
    fn test_hover_attribute_shows_each_top_level_document_that_differs() {
        // Asciidoctor converts each top-level document on its own: both
        // values are correct, neither wins.
        let chapter = "== C\n\nSee {x}.\n";
        let (_tmp, ws, paths) = scanned(&[
            ("a-draft.adoc", "= A\n:x: draft\ninclude::ch.adoc[]\n"),
            ("b-book.adoc", "= B\n:x: book\ninclude::ch.adoc[]\n"),
            ("ch.adoc", chapter),
        ]);
        let hover = resolve_hover(chapter, at_reference(2), &paths[2], Some(&ws)).unwrap();
        assert_eq!(
            hover,
            "In a-draft.adoc:\n:x: draft\n\nDefined in a-draft.adoc:2\n\n\
             In b-book.adoc:\n:x: book\n\nDefined in b-book.adoc:2"
        );
    }

    #[test]
    fn test_hover_image_shows_preview_of_included_file_where_it_breaks() {
        // The book finds the image; previewing the chapter on its own
        // (no config next to it) does not.
        let chapter = "== C\n\nimage::pic.png[]\n";
        let (_tmp, ws, paths) = scanned(&[
            ("book.adoc", "= B\n:imagesdir: img\ninclude::ch/ch.adoc[]\n"),
            ("ch/ch.adoc", chapter),
            ("img/pic.png", ""),
        ]);
        let pos = Position {
            line: 2,
            character: 8,
        };
        let hover = resolve_hover(chapter, pos, &paths[1], Some(&ws)).unwrap();
        assert_eq!(
            hover,
            "In book.adoc:\nimage::img/pic.png[]\n\n\u{2713} File exists\n\n\
             In ch.adoc (preview):\nimage::pic.png[]\n\n\u{2717} File not found"
        );
    }

    #[test]
    fn test_hover_image_shows_one_target_when_preview_config_matches() {
        // The chapter's config points its preview at the book's images.
        let chapter = "== C\n\nimage::pic.png[]\n";
        let (_tmp, ws, paths) = scanned(&[
            ("book.adoc", "= B\n:imagesdir: img\ninclude::ch/ch.adoc[]\n"),
            ("ch/.asciidoctorconfig", ":imagesdir: ../img\n"),
            ("ch/ch.adoc", chapter),
            ("img/pic.png", ""),
        ]);
        let pos = Position {
            line: 2,
            character: 8,
        };
        let hover = resolve_hover(chapter, pos, &paths[2], Some(&ws)).unwrap();
        assert_eq!(hover, "image::img/pic.png[]\n\n\u{2713} File exists");
        let targets = resolve_goto_definitions(chapter, pos, &paths[2], Some(&ws));
        assert_eq!(targets, vec![(paths[3].clone(), 1)]);
    }

    /// An include target takes the attribute values of each top-level
    /// document and of the editor's preview, like an image (ADR-4).
    #[test]
    fn test_hover_include_shows_preview_of_included_file_where_it_breaks() {
        let chapter = "== C\n\ninclude::{parts}/p.adoc[]\n";
        let (_tmp, ws, paths) = scanned(&[
            ("book.adoc", "= B\n:parts: parts\ninclude::ch.adoc[]\n"),
            ("ch.adoc", chapter),
            ("parts/p.adoc", "Text.\n"),
        ]);
        let pos = Position {
            line: 2,
            character: 2,
        };
        let hover = resolve_hover(chapter, pos, &paths[1], Some(&ws)).unwrap();
        assert_eq!(
            hover,
            "In book.adoc:\ninclude::parts/p.adoc[]\n\n\u{2713} File exists | 0 sections\n\n\
             In ch.adoc (preview):\ninclude::{parts}/p.adoc[]\n\n\u{2717} File not found"
        );
        let targets = resolve_goto_definitions(chapter, pos, &paths[1], Some(&ws));
        assert_eq!(targets, vec![(paths[2].clone(), 1)]);
    }

    /// Two top-level documents point the same include at different files:
    /// goto offers both.
    #[test]
    fn test_goto_def_include_offers_each_top_level_document() {
        let chapter = "== C\n\ninclude::{edition}/p.adoc[]\n";
        let (_tmp, ws, paths) = scanned(&[
            ("a-print.adoc", "= A\n:edition: print\ninclude::ch.adoc[]\n"),
            ("b-web.adoc", "= B\n:edition: web\ninclude::ch.adoc[]\n"),
            ("ch.adoc", chapter),
            ("print/p.adoc", "Print.\n"),
            ("web/p.adoc", "Web.\n"),
        ]);
        let pos = Position {
            line: 2,
            character: 2,
        };
        let targets = resolve_goto_definitions(chapter, pos, &paths[2], Some(&ws));
        assert_eq!(targets, vec![(paths[3].clone(), 1), (paths[4].clone(), 1)]);
    }

    /// An xref's file part takes each top-level document's values: goto
    /// offers both targets, hover names the document for each.
    #[test]
    fn test_xref_offers_each_top_level_document() {
        let chapter = "== C\n\nSee xref:{edition}/p.adoc[].\n";
        let (_tmp, ws, paths) = scanned(&[
            ("a-print.adoc", "= A\n:edition: print\ninclude::ch.adoc[]\n"),
            ("b-web.adoc", "= B\n:edition: web\ninclude::ch.adoc[]\n"),
            ("ch.adoc", chapter),
            ("print/p.adoc", "= Print\n"),
            ("web/p.adoc", "= Web\n"),
        ]);
        let pos = Position {
            line: 2,
            character: 6,
        };
        let targets = resolve_goto_definitions(chapter, pos, &paths[2], Some(&ws));
        assert_eq!(targets, vec![(paths[3].clone(), 1), (paths[4].clone(), 1)]);
        let hover = resolve_hover(chapter, pos, &paths[2], Some(&ws)).unwrap();
        assert_eq!(
            hover,
            "In a-print.adoc:\n\u{2192} Print\n\np.adoc:1\n\n\
             In b-web.adoc:\n\u{2192} Web\n\np.adoc:1\n\n\
             In ch.adoc (preview):\n\u{2717} Target not found"
        );
    }

    #[test]
    fn test_hover_attribute_names_top_level_document_without_value() {
        let chapter = "== C\n\nSee {x}.\n";
        let (_tmp, ws, paths) = scanned(&[
            ("a-draft.adoc", "= A\ninclude::ch.adoc[]\n"),
            ("b-book.adoc", "= B\n:x: book\ninclude::ch.adoc[]\n"),
            ("ch.adoc", chapter),
        ]);
        let hover = resolve_hover(chapter, at_reference(2), &paths[2], Some(&ws)).unwrap();
        assert_eq!(
            hover,
            "In a-draft.adoc:\nnot set\n\n\
             In b-book.adoc:\n:x: book\n\nDefined in b-book.adoc:2"
        );
    }

    #[test]
    fn test_hover_attribute_shows_one_value_when_top_level_documents_agree() {
        let chapter = "== C\n\nSee {x}.\n";
        let (_tmp, ws, paths) = scanned(&[
            (
                "a-draft.adoc",
                "= A\ninclude::attrs.adoc[]\ninclude::ch.adoc[]\n",
            ),
            ("attrs.adoc", ":x: shared\n"),
            (
                "b-book.adoc",
                "= B\ninclude::attrs.adoc[]\ninclude::ch.adoc[]\n",
            ),
            ("ch.adoc", chapter),
        ]);
        let hover = resolve_hover(chapter, at_reference(2), &paths[3], Some(&ws)).unwrap();
        assert_eq!(hover, ":x: shared\n\nDefined in attrs.adoc:1");
    }

    #[test]
    fn test_goto_def_attribute_file_included_twice_is_read_twice() {
        let content = "= M\ninclude::a.adoc[]\n:x: two\ninclude::a.adoc[]\n\nSee {x}.\n";
        let (_tmp, ws, paths) = scanned(&[("a.adoc", ":x: one\n"), ("m.adoc", content)]);
        let targets = resolve_goto_definitions(content, at_reference(5), &paths[1], Some(&ws));
        // The second read of a.adoc is the effective one; `:x: two` follows.
        assert_eq!(targets, vec![(paths[0].clone(), 1), (paths[1].clone(), 3)]);
    }

    #[test]
    fn test_goto_def_attribute_in_self_including_file() {
        let content = "= S\ninclude::s.adoc[]\n:x: v\n\nSee {x}.\n";
        let (_tmp, ws, paths) = scanned(&[("s.adoc", content)]);
        let targets = resolve_goto_definitions(content, at_reference(4), &paths[0], Some(&ws));
        assert_eq!(targets, vec![(paths[0].clone(), 3)]);
    }

    #[test]
    fn test_hover_attribute_def_substitutes_references() {
        let content = "= Doc A\n:assets: doc/assets\n:imagesdir: {assets}/img\n";
        let (_tmp, ws, paths) = scanned(&[("a.adoc", content)]);
        let pos = Position {
            line: 2,
            character: 3,
        };

        let hover = resolve_hover(content, pos, &paths[0], Some(&ws)).unwrap();
        assert_eq!(
            hover,
            ":imagesdir: {assets}/img\n\nResolves to doc/assets/img"
        );
    }

    #[test]
    fn test_hover_attribute_unknown_reference_adds_no_resolution() {
        let content = "= Doc A\n:imagesdir: {nowhere}/img\n";
        let (_tmp, ws, paths) = scanned(&[("a.adoc", content)]);
        let pos = Position {
            line: 1,
            character: 3,
        };

        let hover = resolve_hover(content, pos, &paths[0], Some(&ws)).unwrap();
        assert_eq!(hover, ":imagesdir: {nowhere}/img");
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
        assert_eq!(
            resolve_goto_definitions(doc, pos, &a, Some(&ws)),
            vec![(img.join("logo.png"), 1)]
        );
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
        assert!(resolve_goto_definitions(doc, pos, &a, Some(&ws)).is_empty());
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
        assert!(resolve_goto_definitions(doc, pos, &a, Some(&ws)).is_empty());

        let report = explain_position(doc, pos, &a, &a, Some(&ws));
        let res = &report["resolution"]["scopes"][0];
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

    // --- image tests (unchanged) ---

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

        assert!(resolve_goto_definitions(&content, pos, &a, None).is_empty());
    }

    // --- Column-finder helpers ---

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

    // --- prepare_rename tests ---

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

    // --- resolve_rename tests ---

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

    // --- explain_position report ---

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
        assert_eq!(report["resolution"]["name"], "author");
        let scopes = report["resolution"]["scopes"].as_array().unwrap();
        assert_eq!(scopes.len(), 1);
        let res = &scopes[0];
        assert_eq!(res["scope"], "a.adoc");
        assert_eq!(res["value"], "Alice");
        assert_eq!(res["definition"]["origin"], "document");
        assert_eq!(res["definition"]["raw"], "Alice");
        assert_eq!(res["definition"]["source"]["line"], 2);
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
        // The value before the entry: on its own line it is not set yet.
        let res = &report["resolution"]["scopes"][0];
        assert!(res["value"].is_null());
        assert_eq!(res["definition"]["origin"], "document");
        assert_eq!(res["definition"]["value"], "Alice");
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
        assert_eq!(
            report["resolution"]["scopes"][0]["resolved"]["file"],
            path_string(&b)
        );
        assert_eq!(report["resolution"]["scopes"][0]["resolved"]["line"], 1);
        assert!(report["resolution"]["scopes"][0]["error"].is_null());
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
        assert!(report["resolution"]["scopes"][0]["resolved"].is_null());
        assert_eq!(
            report["resolution"]["scopes"][0]["error"],
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
        assert_eq!(report["resolution"]["scopes"][0]["target"], path_string(&b));
        assert_eq!(report["resolution"]["scopes"][0]["exists"], true);
        assert_eq!(
            report["resolution"]["scopes"][0]["canonical_target"],
            path_string(&b)
        );
        assert_eq!(
            resolve_goto_definitions(&content, pos, &a, Some(&ws)),
            vec![(b, 1)]
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
            report["resolution"]["scopes"][0]["target"],
            path_string(&root.join("missing.adoc"))
        );
        assert_eq!(report["resolution"]["scopes"][0]["exists"], false);
        assert!(report["resolution"]["scopes"][0]["canonical_target"].is_null());
        assert!(resolve_goto_definitions(&content, pos, &a, Some(&ws)).is_empty());
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

        let res = &report["resolution"]["scopes"][0];
        assert_eq!(
            res["target"],
            path_string(&root.join("link").join("sub.adoc"))
        );
        assert_eq!(res["canonical_target"], path_string(&sub));
        assert_eq!(
            resolve_goto_definitions(&content, pos, &a, None),
            vec![(sub, 1)]
        );
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
            report["resolution"]["scopes"][0]["target"],
            path_string(&root.join("img").join("logo.png"))
        );
        assert_eq!(report["resolution"]["scopes"][0]["display"], "img/logo.png");
        assert_eq!(report["resolution"]["scopes"][0]["exists"], false);
        assert_eq!(report["resolution"]["scopes"][0]["remote"], false);
        assert!(report["resolution"]["scopes"][0]["canonical_target"].is_null());
        assert!(resolve_goto_definitions(&content, pos, &a, Some(&ws)).is_empty());
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

        let res = &report["resolution"]["scopes"][0];
        assert_eq!(
            res["target"],
            path_string(&root.join("link").join("logo.png"))
        );
        assert_eq!(res["canonical_target"], path_string(&logo));
        assert_eq!(
            resolve_goto_definitions(&content, pos, &a, Some(&ws)),
            vec![(logo, 1)]
        );
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
        assert_eq!(author["scope"], "a.adoc");
        assert_eq!(author["origin"], "document");
        assert_eq!(author["source"]["line"], 2);
        let docdir = known.iter().find(|k| k["name"] == "docdir").unwrap();
        assert_eq!(docdir["origin"], "builtin");
    }

    #[test]
    fn test_explain_origin_names_are_stable_snake_case() {
        assert_eq!(origin_name(AttributeOrigin::Document), "document");
        assert_eq!(origin_name(AttributeOrigin::Config), "config");
        assert_eq!(origin_name(AttributeOrigin::Builtin), "builtin");
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
        assert_eq!(report["resolution"]["scopes"][0]["value"], "Alice");

        let fixed = explain_position(&content, pos, &raw, &a, Some(&ws));

        assert_eq!(fixed["request"]["raw_path"], path_string(&raw));
        assert_eq!(fixed["request"]["lookup_path"], path_string(&a));
        assert_eq!(fixed["resolution"]["scopes"][0]["value"], "Alice");
    }
}
