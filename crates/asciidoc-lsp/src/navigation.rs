use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use tower_lsp_server::ls_types::Position;

use asciidoc_core::anchor_scope::{AnchorScope, CatalogCache};
use asciidoc_core::attributes::{config_attribute_at, AttributeSource};
use asciidoc_core::diagnostics::display_path;
use asciidoc_core::extractors::{element_attr_id, find_child_text, is_valid_anchor_id};
use asciidoc_core::paths::{canonicalize_if_exists, is_uri, join_doc_path, normalize_lexically};
use asciidoc_core::pdf_theme::{is_builtin_theme, is_pdf_path_attribute, PdfPath, PDF_THEME};
use asciidoc_core::references::{parse_xref_target, xref_names_file};
use asciidoc_core::types::{
    AnchorKind, AnchorLocation, DiagnosticKind, FileLocation, ResolvedXref,
};
use asciidoc_core::workspace::{
    entry_name, AttributeDefinition, AttributeOrigin, AttributeScope, ConfigOverride, Workspace,
};

use crate::positions;
use crate::scopes::{
    collapse_if_agree, document_dir, file_display_name, path_scopes, scope_docdir,
    scope_image_base, scope_label, unique_in_order, DocTarget,
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
    /// An attribute entry line; `unset` for `:name!:` / `:!name:`.
    AttributeDef { name: String, unset: bool },
    /// `[[id]]` anchor / ID assignment
    Anchor { id: String },
    /// `image::path[]` block macro or `image:path[]` inline macro; `attrlist`
    /// is the text between `[` and `]`, as written
    Image {
        path: String,
        inline: bool,
        attrlist: String,
    },
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
    // Parse each range containing the cursor on its own: one range's markup
    // must not pair with another's (e.g. a `+` in one paragraph matching a `+`
    // in another, creating a giant false passthrough node that swallows xref
    // nodes).
    let inline_ranges = asciidoc_core::extractors::collect_inline_ranges(
        block_tree.root_node(),
        content.as_bytes(),
    );
    let mut inline_parser = asciidoc_core::parser::init_inline_parser().ok()?;
    for range in inline_ranges
        .iter()
        .filter(|r| r.start_point.row <= point.row && r.end_point.row >= point.row)
    {
        if let Some(info) = classify_in_range(&mut inline_parser, *range, content, point) {
            return Some(info);
        }
    }

    None
}

/// Parse `range` with the inline grammar and classify the node at `point`.
///
/// Footnote content is opaque in the inline grammar, so when the cursor sits
/// inside a footnote its content range is parsed on its own and classified.
fn classify_in_range(
    parser: &mut tree_sitter::Parser,
    range: tree_sitter::Range,
    content: &str,
    point: tree_sitter::Point,
) -> Option<NodeInfo> {
    parser.set_included_ranges(&[range]).ok()?;
    let tree = asciidoc_core::parser::parse(parser, content).ok()?;
    if let Some(info) = classify_inline_node(tree.root_node(), point, content.as_bytes()) {
        return Some(info);
    }
    let footnote_range = asciidoc_core::extractors::footnote_content_ranges(tree.root_node())
        .into_iter()
        .find(|r| r.start_point <= point && point <= r.end_point)?;
    parser.set_included_ranges(&[footnote_range]).ok()?;
    let footnote_tree = asciidoc_core::parser::parse(parser, content).ok()?;
    classify_inline_node(footnote_tree.root_node(), point, content.as_bytes())
}

/// Resolve a go-to-definition request to target files and lines
/// (1-indexed). An attribute reference yields every definition, the one
/// Asciidoctor uses first; an include, xref or image yields each scope's
/// existing target once, the preview of an included file none (ADR-4).
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
                resolve_image_targets(
                    path,
                    doc_path,
                    workspace,
                    node_info.line,
                    Readings::Documents,
                )
                .into_iter()
                .filter_map(|(_, target)| match target {
                    ImageTarget::Local { path: target, .. } => target.canonicalize().ok(),
                    ImageTarget::Remote { .. } => None,
                })
                .map(|canonical| (canonical, 1)),
            )
        }
        NodeKind::Include { ref path } => {
            let targets = resolve_doc_targets(
                path,
                doc_path,
                workspace,
                node_info.line,
                Readings::Documents,
            );
            existing_targets(&targets)
                .into_iter()
                .map(|file| (file, 1))
                .collect()
        }
        NodeKind::Xref { ref target } | NodeKind::ShorthandXref { ref target } => {
            let Some(ws) = workspace else {
                return Vec::new();
            };
            let catalogs = CatalogCache::new(ws);
            unique_in_order(
                resolve_xref_targets(
                    ws,
                    &catalogs,
                    doc_path,
                    target,
                    node_info.line,
                    Readings::Documents,
                )
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
/// the definition location is included in the results. `overrides` holds the
/// content of unsaved buffers, which spans on anchor declarations are read from.
pub fn resolve_references(
    content: &str,
    pos: Position,
    doc_path: &Path,
    workspace: Option<&Workspace>,
    include_declaration: bool,
    overrides: &HashMap<PathBuf, Arc<String>>,
) -> Option<Vec<FileLocation>> {
    let node_info = find_node_at_position(content, pos)?;
    let ws = workspace?;
    let catalogs = CatalogCache::new(ws);

    let mut results = match &node_info.kind {
        NodeKind::AttributeRef { name } | NodeKind::AttributeDef { name, .. } => {
            let mut r = ws.find_all_attribute_usages(name);
            if include_declaration {
                for source in ws.definition_sites(doc_path, node_info.line, name) {
                    r.push(FileLocation::new(source.file, source.line, 0, 0));
                }
            }
            r
        }
        NodeKind::Include { path } => {
            let targets = resolve_doc_targets(
                path,
                doc_path,
                Some(ws),
                node_info.line,
                Readings::Documents,
            );
            let mut r = Vec::new();
            for target in existing_targets(&targets) {
                let filename = target.file_name().and_then(|f| f.to_str()).unwrap_or("");
                r.extend(ws.find_all_include_references(&target));
                r.extend(ws.find_all_file_xrefs(filename));
            }
            r
        }
        NodeKind::Anchor { id } => {
            let definitions = anchor_definitions_of_anchor(ws, &catalogs, doc_path, id);
            anchor_reference_locations(&catalogs, id, &definitions, include_declaration, overrides)
        }
        NodeKind::Xref { target } | NodeKind::ShorthandXref { target } => {
            // A file-only xref has no anchor to find references for.
            let anchor_id = xref_anchor_id(target)?;
            let definitions = anchor_definitions_of_xref(
                ws,
                &catalogs,
                doc_path,
                target,
                anchor_id,
                node_info.line,
            );
            anchor_reference_locations(
                &catalogs,
                anchor_id,
                &definitions,
                include_declaration,
                overrides,
            )
        }
        NodeKind::Image { .. } => {
            // Images have no references semantics — find-references is a no-op
            return None;
        }
    };

    // Deduplicate results (same file+line can appear from multiple sources)
    results.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.line.cmp(&b.line))
            .then(a.col.cmp(&b.col))
    });
    results.dedup_by(|a, b| a.path == b.path && a.line == b.line && a.col == b.col);

    if results.is_empty() {
        None
    } else {
        Some(results)
    }
}

/// The anchor id an xref target names: the part after `#`, or a plain id.
/// `None` for a file-only target or an empty anchor.
fn xref_anchor_id(target: &str) -> Option<&str> {
    if let Some((_file, anchor)) = target.split_once('#') {
        (!anchor.is_empty()).then_some(anchor)
    } else if target.contains('.') {
        None
    } else {
        Some(target)
    }
}

/// The span of the xref to `anchor_id` on `line` of `doc_path` closest to the
/// byte column `cursor_col`, as the workspace extracted it.
fn xref_span_near(
    ws: &Workspace,
    doc_path: &Path,
    anchor_id: &str,
    line: usize,
    cursor_col: usize,
) -> Option<(usize, usize)> {
    ws.documents()
        .get(doc_path)?
        .xrefs
        .iter()
        .filter(|xref| xref.line == line)
        .filter_map(asciidoc_core::types::Xref::id_span)
        .filter(|(id, _, _)| *id == anchor_id)
        .min_by_key(|&(_, col, end_col)| {
            if cursor_col < col {
                col - cursor_col
            } else {
                cursor_col.saturating_sub(end_col)
            }
        })
        .map(|(_, col, end_col)| (col, end_col))
}

/// Where go-to-definition lands, as locations: from an xref, the id inside
/// the anchor's `[[id]]` (the line start when no line spells it); anywhere
/// else the line start of each target of [`resolve_goto_definitions`].
pub fn resolve_definition_locations(
    content: &str,
    pos: Position,
    doc_path: &Path,
    workspace: Option<&Workspace>,
    overrides: &HashMap<PathBuf, Arc<String>>,
) -> Vec<FileLocation> {
    let anchor_id = match find_node_at_position(content, pos).map(|info| info.kind) {
        Some(NodeKind::Xref { target } | NodeKind::ShorthandXref { target }) => {
            xref_anchor_id(&target).map(str::to_string)
        }
        _ => None,
    };
    let mut file_cache: HashMap<PathBuf, String> = HashMap::new();
    resolve_goto_definitions(content, pos, doc_path, workspace)
        .into_iter()
        .map(|(path, line)| {
            let (col, end_col) = anchor_id
                .as_deref()
                .and_then(|id| {
                    let text = read_file_cached(&mut file_cache, &path, overrides);
                    let line_text = text.lines().nth(line.saturating_sub(1))?;
                    find_anchor_def_columns(line_text, id)
                })
                .unwrap_or((0, 0));
            FileLocation::new(path, line, col, end_col)
        })
        .collect()
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
/// - `AttributeDef`: value, with references replaced as for `AttributeRef`;
///   for an attribute of the completion catalog also its description and
///   choice values
/// - `pdf-theme`, `pdf-themesdir`, `pdf-fontsdir` (reference or definition):
///   resolved absolute path
/// - `Xref`/`ShorthandXref`: section title + file, or `✗ Target not found`
/// - `Anchor`: what an xref to its ID shows from there
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
        // An unset entry resolves to no paths; it falls through to the generic arm.
        NodeKind::AttributeDef { name, unset: false } if is_pdf_path_attribute(name) => {
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
        NodeKind::AttributeDef { name, .. } => {
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
            if texts.is_empty() {
                return None;
            }
            let hover = per_document_hover(texts);
            Some(match catalog_attribute_doc(name) {
                Some(doc) => format!("{hover}\n\n{doc}"),
                None => hover,
            })
        }
        NodeKind::Xref { target } | NodeKind::ShorthandXref { target } => {
            let ws = workspace?;
            let catalogs = CatalogCache::new(ws);
            let resolved = resolve_xref_targets(
                ws,
                &catalogs,
                doc_path,
                target,
                node_info.line,
                Readings::WithPreview,
            );
            if resolved.is_empty() {
                return None;
            }
            let texts: Vec<(String, String)> = resolved
                .into_iter()
                .map(|(label, resolved)| {
                    let text = resolved.map_or_else(
                        |_| "\u{2717} Target not found".to_string(),
                        |resolved| xref_hover(ws, &resolved),
                    );
                    (label, text)
                })
                .collect();
            Some(per_document_hover(texts))
        }
        NodeKind::Include { path } => {
            let mut targets = resolve_doc_targets(
                path,
                doc_path,
                workspace,
                node_info.line,
                Readings::WithPreview,
            );
            // Scopes that reach the same file agree, however they spell it.
            collapse_if_agree(&mut targets, |a, b| a.path == b.path);
            let texts: Vec<(String, String)> = targets
                .into_iter()
                .map(|target| {
                    let mut text = format!("include::{}[]", target.substituted);
                    if is_uri(&target.substituted) {
                        text.push_str("\n\n\u{2197} Remote include, not fetched");
                    } else if target.path.exists() {
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
        NodeKind::Image {
            path,
            inline,
            attrlist,
        } => {
            let macro_form = if *inline { "image:" } else { "image::" };
            let mut targets = resolve_image_targets(
                path,
                doc_path,
                workspace,
                node_info.line,
                Readings::WithPreview,
            );
            // Scopes that reach the same file agree, however they spell it.
            collapse_if_agree(&mut targets, |a, b| a.1.same_place(&b.1));
            let texts: Vec<(String, String)> = targets
                .into_iter()
                .map(|(label, target)| {
                    let text = match target {
                        ImageTarget::Remote { url } => format!("{macro_form}{url}[{attrlist}]"),
                        ImageTarget::Local {
                            path: target,
                            display,
                        } => {
                            let status = if target.exists() {
                                "\u{2713} File exists"
                            } else {
                                "\u{2717} File not found"
                            };
                            format!("{macro_form}{display}[{attrlist}]\n\n{status}")
                        }
                    };
                    (label, text)
                })
                .collect();
            Some(per_document_hover(texts))
        }
        NodeKind::Anchor { id } => {
            let ws = workspace?;
            let catalogs = CatalogCache::new(ws);
            let texts: Vec<(String, String)> = resolve_xref_targets(
                ws,
                &catalogs,
                doc_path,
                id,
                node_info.line,
                Readings::WithPreview,
            )
            .into_iter()
            .filter_map(|(label, resolved)| Some((label, xref_hover(ws, &resolved.ok()?))))
            .collect();
            (!texts.is_empty()).then(|| per_document_hover(texts))
        }
    }
}

/// Hover text for an xref that resolves: the title of the section that
/// carries the ID (`Workspace::anchor_title`) or, for an xref that names only
/// a file, of its document header; then file and line. An ID outside a
/// section title shows the anchor instead, a file without a header only the
/// place.
fn xref_hover(ws: &Workspace, resolved: &ResolvedXref) -> String {
    let id = resolved.id.as_deref();
    let title = match (resolved.kind, id) {
        (Some(kind), Some(id)) => ws.anchor_title(
            id,
            &AnchorLocation {
                file: resolved.file.clone(),
                line: resolved.line,
                kind,
            },
        ),
        _ => ws.documents().get(&resolved.file).and_then(|doc| {
            doc.sections
                .iter()
                .find(|s| s.is_header)
                .map(|s| s.title.as_str())
        }),
    };
    let place = format!("{}:{}", file_display_name(&resolved.file), resolved.line);
    match (title, id) {
        (Some(title), _) => format!("\u{2192} {title}\n\n{place}"),
        (None, Some(id)) => format!("[[{id}]]\n\n{place}"),
        (None, None) => place,
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

/// Description and choice values of a completion-catalog document attribute.
fn catalog_attribute_doc(name: &str) -> Option<String> {
    let (detail, values) = crate::completion::doc_attribute_doc(name)?;
    if values.is_empty() {
        Some(detail.to_string())
    } else {
        Some(format!("{detail}\n\nValues: {}", values.join(", ")))
    }
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

/// Hover text for one definition: the entry (`:name!:` for either unset form,
/// as `:!name:` is shown normalised), then what it resolves to where it differs.
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
/// a file others include and when `readings` asks for it, in the editor's
/// preview of it (ADR-4): target, `imagesdir` and `docdir` take the values
/// in effect there. Each target comes with its scope's label; without a
/// workspace, one target with the label of `doc_path`.
fn resolve_image_targets(
    image_path: &str,
    doc_path: &Path,
    workspace: Option<&Workspace>,
    line: usize,
    readings: Readings,
) -> Vec<(String, ImageTarget)> {
    let file_dir = document_dir(doc_path, workspace);
    let Some(ws) = workspace else {
        let target = image_target(image_path, file_dir, file_dir, "");
        return vec![(file_display_name(doc_path), target)];
    };
    path_scopes(ws, doc_path, line)
        .filter(|scope| readings == Readings::WithPreview || !scope.is_preview())
        .map(|scope| {
            let (base, imagesdir) = scope_image_base(&scope, file_dir);
            let doc_dir = scope_docdir(&scope, file_dir);
            let target = image_target(&scope.substitute(image_path), &base, &doc_dir, &imagesdir);
            (scope_label(&scope), target)
        })
        .collect()
}

/// An image target from its substituted path, the folder it resolves
/// against and the `imagesdir` shown in front of it. An absolute path shown
/// is shown relative to `doc_dir` as far as `display_path` allows.
fn image_target(resolved_path: &str, base: &Path, doc_dir: &Path, imagesdir: &str) -> ImageTarget {
    let mut display = if imagesdir.is_empty() || is_remote_url(resolved_path) {
        resolved_path.to_string()
    } else {
        format!("{imagesdir}/{resolved_path}")
    };
    if Path::new(&display).is_absolute() {
        display = display_path(doc_dir, Path::new(&display))
            .display()
            .to_string();
    }
    if is_remote_url(&display) {
        return ImageTarget::Remote { url: display };
    }
    ImageTarget::Local {
        path: join_doc_path(base, resolved_path),
        display,
    }
}

/// Where an include or xref file path on `line` points, per top-level
/// document and, for a file others include and when `readings` asks for
/// it, in the editor's preview of it (ADR-4), without checking that it
/// exists. Each target comes with its scope's label and the path as
/// substituted there; canonical when it exists. Without a workspace, one
/// target with the label of `doc_path`.
fn resolve_doc_targets(
    target_path: &str,
    doc_path: &Path,
    workspace: Option<&Workspace>,
    line: usize,
    readings: Readings,
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
        .filter(|scope| readings == Readings::WithPreview || !scope.is_preview())
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

/// Where the ID `id` is defined for a cursor on a definition in `doc_path`:
/// its first definition in each top-level document that reads the file, and
/// the explicit definitions after it, which a rename keeps in step.
fn anchor_definitions_of_anchor(
    ws: &Workspace,
    catalogs: &CatalogCache<'_>,
    doc_path: &Path,
    id: &str,
) -> Vec<AnchorLocation> {
    let mut definitions = Vec::new();
    for catalog in catalogs.scopes_at(doc_path) {
        let repeated = catalog
            .duplicates()
            .iter()
            .filter(|duplicate| duplicate.id == id)
            .map(|duplicate| &duplicate.second);
        for location in catalog.lookup(id).into_iter().chain(repeated) {
            push_definition(&mut definitions, location.clone());
        }
    }
    close_definitions(ws, catalogs, id, definitions)
}

/// Where the xref `target` on `line` of `doc_path` finds its ID, in each
/// top-level document that reads the file, closed as [`close_definitions`].
fn anchor_definitions_of_xref(
    ws: &Workspace,
    catalogs: &CatalogCache<'_>,
    doc_path: &Path,
    target: &str,
    id: &str,
    line: usize,
) -> Vec<AnchorLocation> {
    let definitions = xref_definitions(ws, catalogs, doc_path, target, id, line);
    close_definitions(ws, catalogs, id, definitions)
}

/// `definitions` of `id` plus every definition the xrefs that reach them
/// reach in other top-level documents: an xref in a chapter two books share
/// finds a definition in each, and renaming only one would break the other.
/// Repeats until nothing is added, so the result is the same wherever a
/// rename or find-references starts. An xref reaches the same definitions
/// whatever is known, so each reference is followed once, when it first
/// shows up.
fn close_definitions(
    ws: &Workspace,
    catalogs: &CatalogCache<'_>,
    id: &str,
    mut definitions: Vec<AnchorLocation>,
) -> Vec<AnchorLocation> {
    let mut followed: HashSet<(PathBuf, usize, usize)> = HashSet::new();
    loop {
        let known = definitions.len();
        for reference in catalogs.anchor_references(id, &definitions) {
            if !followed.insert((reference.path.clone(), reference.line, reference.col)) {
                continue;
            }
            let Some(xref) = ws.documents().get(&reference.path).and_then(|doc| {
                doc.xrefs.iter().find(|xref| {
                    xref.line == reference.line
                        && xref
                            .id_span()
                            .is_some_and(|(_, col, _)| col == reference.col)
                })
            }) else {
                continue;
            };
            for location in
                xref_definitions(ws, catalogs, &reference.path, &xref.target, id, xref.line)
            {
                push_definition(&mut definitions, location);
            }
        }
        if definitions.len() == known {
            return definitions;
        }
    }
}

/// Where the xref `target` on `line` of `doc_path` finds its ID, in each
/// top-level document that reads the file, with the explicit definitions
/// that follow it.
fn xref_definitions(
    ws: &Workspace,
    catalogs: &CatalogCache<'_>,
    doc_path: &Path,
    target: &str,
    id: &str,
    line: usize,
) -> Vec<AnchorLocation> {
    let mut definitions = Vec::new();
    for (_, resolved) in
        resolve_xref_targets(ws, catalogs, doc_path, target, line, Readings::Documents)
    {
        if let Ok(ResolvedXref {
            file,
            line,
            kind: Some(kind),
            ..
        }) = resolved
        {
            push_definition(&mut definitions, AnchorLocation { file, line, kind });
        }
    }
    // The explicit definitions that follow a found one in its document are
    // the same ID defined again, which a rename keeps in step. The document
    // is the one the xref resolved in: that of the reading file, or, for a
    // link to another document, one that reads the definition's file or the
    // definition's file on its own.
    let scopes = catalogs
        .scopes_at(doc_path)
        .into_iter()
        .chain(definitions.iter().flat_map(|d| {
            let mut scopes = catalogs.scopes_at(&d.file);
            scopes.push(catalogs.get(&d.file));
            scopes
        }))
        .collect::<Vec<_>>();
    for catalog in &scopes {
        let found = catalog.lookup(id).is_some_and(|first| {
            definitions
                .iter()
                .any(|known| known.file == first.file && known.line == first.line)
        });
        if found {
            for duplicate in catalog.duplicates().iter().filter(|d| d.id == id) {
                push_definition(&mut definitions, duplicate.second.clone());
            }
        }
    }
    definitions
}

/// Adds `location` unless a definition on that line is known already.
fn push_definition(definitions: &mut Vec<AnchorLocation>, location: AnchorLocation) {
    if !definitions
        .iter()
        .any(|known| known.file == location.file && known.line == location.line)
    {
        definitions.push(location);
    }
}

/// The xrefs that reach any of `definitions` of `id`, and the definitions
/// too when `include_declaration` is set, each at the span of the id on its
/// line (the spans rename edits). An xref carries its span from extraction;
/// the declaration's is found in its line (`[[id]]` or `[[id,reftext]]`). A
/// declaration whose line holds no such span, such as a section's implicit
/// id, stays a zero-width location at the line start.
fn anchor_reference_locations(
    catalogs: &CatalogCache<'_>,
    id: &str,
    definitions: &[AnchorLocation],
    include_declaration: bool,
    overrides: &HashMap<PathBuf, Arc<String>>,
) -> Vec<FileLocation> {
    let mut file_cache: HashMap<PathBuf, String> = HashMap::new();
    let mut locations = catalogs.anchor_references(id, definitions);
    if include_declaration {
        for definition in definitions {
            let text = read_file_cached(&mut file_cache, &definition.file, overrides);
            let line_text = text
                .lines()
                .nth(definition.line.saturating_sub(1))
                .unwrap_or_default();
            let (col, end_col) = find_anchor_def_columns(line_text, id).unwrap_or((0, 0));
            locations.push(FileLocation::new(
                definition.file.clone(),
                definition.line,
                col,
                end_col,
            ));
        }
    }
    // One entry per span: a rename edits each once.
    locations.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.line.cmp(&b.line))
            .then(a.col.cmp(&b.col))
    });
    locations.dedup_by(|a, b| a.path == b.path && a.line == b.line && a.col == b.col);
    locations
}

/// How an xref on `line` resolves, per top-level document, with each
/// document's own ID catalog. The file part is read from the folder of the
/// document, as Asciidoctor reads it, also in an included file. A target
/// with a file part also resolves
/// in the editor's preview of a file others include (ADR-4): the path as
/// the preview sees it, and the ID in the catalog of that file read alone,
/// which is what Asciidoctor knows when it converts the file on its own. A
/// bare ID has no preview, where every reference to a neighbouring chapter
/// would be broken. `readings` says whether the preview takes part. Each
/// result comes with its scope's label. Shared by go-to-definition, hover,
/// find-references, rename and the explain report.
fn resolve_xref_targets(
    ws: &Workspace,
    catalogs: &CatalogCache<'_>,
    doc_path: &Path,
    target: &str,
    line: usize,
    readings: Readings,
) -> Vec<(String, Result<ResolvedXref, DiagnosticKind>)> {
    let xref = asciidoc_core::types::Xref {
        target: target.to_string(),
        display_text: None,
        line,
        span: None,
        target_col: 0,
    };
    // A document that reads `doc_path` only in part is no judge of what is
    // missing there (the walk knows no tag regions): a failure is dropped.
    let known = |catalog: &AnchorScope, resolved: Result<ResolvedXref, DiagnosticKind>| {
        (resolved.is_ok() || catalog.reads_whole(doc_path)).then_some(resolved)
    };
    if !xref_names_file(target) {
        return catalogs
            .scopes_at(doc_path)
            .iter()
            .filter_map(|catalog| {
                let resolved = catalogs.resolve_xref_in(catalog, &xref, |_| None);
                known(catalog, resolved)
                    .map(|resolved| (file_display_name(catalog.document()), resolved))
            })
            .collect();
    }
    if target.contains('{') {
        // An attribute reference in the file part reads differently in each
        // scope, so each scope is read for its own attributes.
        return path_scopes(ws, doc_path, line)
            .filter(|scope| readings == Readings::WithPreview || !scope.is_preview())
            .filter_map(|scope| {
                let catalog = catalogs.get(scope.document());
                let dir = document_dir(scope.document(), Some(ws));
                let resolved = catalogs.resolve_xref_in(&catalog, &xref, |file| {
                    Some(canonicalize_if_exists(join_doc_path(
                        dir,
                        &scope.substitute(file),
                    )))
                });
                known(&catalog, resolved).map(|resolved| (scope_label(&scope), resolved))
            })
            .collect();
    }
    // The same scopes as `path_scopes`: one per top-level document and, for a
    // file others include, its preview, which reads the file alone.
    let preview = (readings == Readings::WithPreview && catalogs.is_included(doc_path))
        .then(|| catalogs.get(doc_path));
    catalogs
        .scopes_at(doc_path)
        .into_iter()
        .map(|catalog| (file_display_name(catalog.document()), catalog))
        .chain(preview.map(|catalog| {
            let label = format!("{} (preview)", file_display_name(catalog.document()));
            (label, catalog)
        }))
        .filter_map(|(label, catalog)| {
            let dir = document_dir(catalog.document(), Some(ws));
            let resolved = catalogs.resolve_xref_in(&catalog, &xref, |file| {
                Some(canonicalize_if_exists(join_doc_path(dir, file)))
            });
            known(&catalog, resolved).map(|resolved| (label, resolved))
        })
        .collect()
}

/// Which readings of a file an xref, include or image resolves in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Readings {
    /// The top-level documents that read the file: the targets goto,
    /// find-references and rename follow. A target only the preview finds
    /// is broken where the file is converted, and following it would
    /// contradict what Asciidoctor uses there.
    Documents,
    /// Those and, for a file others include, the editor's preview of it:
    /// what hover shows.
    WithPreview,
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
        NodeKind::AttributeRef { name } | NodeKind::AttributeDef { name, .. } => {
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
        NodeKind::AttributeRef { name } | NodeKind::AttributeDef { name, .. } => workspace
            .map_or_else(no_workspace, |ws| {
                explain_attribute(ws, doc_path, name, info.line)
            }),
        NodeKind::Xref { target } | NodeKind::ShorthandXref { target } => workspace
            .map_or_else(no_workspace, |ws| {
                explain_xref(ws, doc_path, target, info.line)
            }),
        NodeKind::Anchor { id } => workspace.map_or_else(no_workspace, |ws| {
            let scopes: Vec<Value> = ws
                .anchor_scopes_at(doc_path)
                .iter()
                .map(|catalog| {
                    let definition = catalog.lookup(id).map(|loc| {
                        json!({
                            "file": path_string(&loc.file),
                            "line": loc.line,
                            "kind": anchor_kind_name(loc.kind),
                        })
                    });
                    json!({
                        "scope": file_display_name(catalog.document()),
                        "definition": definition,
                    })
                })
                .collect();
            json!({ "scopes": scopes })
        }),
        NodeKind::Include { path } => {
            let scopes: Vec<Value> =
                resolve_doc_targets(path, doc_path, workspace, info.line, Readings::WithPreview)
                    .into_iter()
                    .map(|target| {
                        if is_uri(&target.substituted) {
                            return json!({
                                "scope": target.label,
                                "remote": true,
                                "target": null,
                                "canonical_target": null,
                                "display": target.substituted,
                                "exists": null,
                            });
                        }
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
            let scopes: Vec<Value> =
                resolve_image_targets(path, doc_path, workspace, info.line, Readings::WithPreview)
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
    let catalogs = CatalogCache::new(ws);
    let scopes: Vec<Value> =
        resolve_xref_targets(ws, &catalogs, doc_path, target, line, Readings::WithPreview)
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
        // [#id] / [id=...] attribute list, only with the cursor on the ID
        if n.kind() == "element_attr" {
            if let Some((id, _)) = element_attr_id(n, source) {
                if cursor_on_attr_id(node, source, &id) {
                    return Some(NodeInfo {
                        kind: NodeKind::Anchor { id },
                        line: n.start_position().row + 1,
                    });
                }
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
                            attrlist: macro_attrlist(n, source),
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

/// Whether `node`, a leaf inside an attribute list, is the `#id` shorthand
/// or the value of `id=` and reads `id`.
fn cursor_on_attr_id(node: tree_sitter::Node, source: &[u8], id: &str) -> bool {
    let mut current = Some(node);
    while let Some(n) = current {
        let is_id = match n.kind() {
            "id" => true,
            "attribute_value" => n.parent().is_some_and(|attr| {
                find_child_text(attr, "attribute_name", source).as_deref() == Some("id")
            }),
            "element_attr" => return false,
            _ => false,
        };
        if is_id {
            return n
                .utf8_text(source)
                .is_ok_and(|text| text.trim_matches(['"', '\'']) == id);
        }
        current = n.parent();
    }
    false
}

/// The text of the macro `node` between its `[` and its `]`, as written.
/// Empty when either bracket is missing, as in a macro still being typed.
fn macro_attrlist(node: tree_sitter::Node, source: &[u8]) -> String {
    let mut cursor = node.walk();
    let children: Vec<_> = node.children(&mut cursor).collect();
    let open = children.iter().find(|c| c.kind() == "[" && !c.is_missing());
    let close = children
        .iter()
        .rfind(|c| c.kind() == "]" && !c.is_missing());
    let (Some(open), Some(close)) = (open, close) else {
        return String::new();
    };
    if close.start_byte() < open.end_byte() {
        return String::new();
    }
    String::from_utf8_lossy(&source[open.end_byte()..close.start_byte()]).into_owned()
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
                if macro_name == "anchor" {
                    let id = find_child_text(n, "target", source).unwrap_or_default();
                    if asciidoc_core::extractors::is_valid_anchor_id(&id) {
                        return Some(NodeInfo {
                            kind: NodeKind::Anchor { id },
                            line: n.start_position().row + 1,
                        });
                    }
                }
                if macro_name == "image" {
                    let path = find_child_text(n, "target", source).unwrap_or_default();
                    return Some(NodeInfo {
                        kind: NodeKind::Image {
                            path,
                            inline: true,
                            attrlist: macro_attrlist(n, source),
                        },
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

/// Split an attribute entry line into its name and whether it unsets.
///
/// The line shape is checked here; the `!` rule is core's `entry_name`, so
/// both crates agree on which entries unset.
fn parse_attribute_entry(line_str: &str) -> Option<(&str, bool)> {
    let rest = line_str.strip_prefix(':')?;
    let (inner, after) = rest.split_once(':')?;
    // After the closing `:` must be end-of-line or whitespace
    if !after.is_empty() && !after.starts_with([' ', '\t']) {
        return None;
    }
    let (name, unset) = entry_name(inner);
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    valid.then_some((name, unset))
}

/// Detect `:name: value` attribute definitions on the current line, including
/// unset entries (`:name!:`, `:!name:`; the name excludes the `!`).
/// Returns `NodeInfo` if the line matches.
fn find_attribute_def_at_position(content: &str, line_1: usize) -> Option<NodeInfo> {
    let line_str = content.lines().nth(line_1.saturating_sub(1))?;
    let (name, unset) = parse_attribute_entry(line_str)?;
    Some(NodeInfo {
        kind: NodeKind::AttributeDef {
            name: name.to_string(),
            unset,
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
    doc_path: &Path,
    workspace: Option<&Workspace>,
) -> Option<PrepareRenameResult> {
    let node_info = find_node_at_position(content, pos)?;
    let line_0 = node_info.line.saturating_sub(1);
    let line_text = content.lines().nth(line_0)?;

    match &node_info.kind {
        NodeKind::Anchor { id } => {
            // Reject implicit section IDs, and an anchor no catalog defines.
            if let Some(ws) = workspace {
                let catalogs = CatalogCache::new(ws);
                let definitions = anchor_definitions_of_anchor(ws, &catalogs, doc_path, id);
                if definitions.is_empty()
                    || definitions
                        .iter()
                        .any(|loc| loc.kind == AnchorKind::ImplicitSection)
                {
                    return None;
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
            let anchor_id = parse_xref_target(target).1?;
            // Reject implicit section IDs, and an xref no definition answers:
            // a rename would find no edit.
            if let Some(ws) = workspace {
                let catalogs = CatalogCache::new(ws);
                let definitions = anchor_definitions_of_xref(
                    ws,
                    &catalogs,
                    doc_path,
                    target,
                    anchor_id,
                    node_info.line,
                );
                if definitions.is_empty()
                    || definitions
                        .iter()
                        .any(|loc| loc.kind == AnchorKind::ImplicitSection)
                    || names_no_id(&catalogs, doc_path, target, anchor_id)
                {
                    return None;
                }
            }
            // Only the xref under the cursor: another one on the line may
            // name the same ID and reach a different definition.
            let (_, cursor_col) = positions::from_lsp_position(pos, content);
            let (start, end) = workspace
                .and_then(|ws| xref_span_near(ws, doc_path, anchor_id, node_info.line, cursor_col))
                .or_else(|| find_anchor_ref_columns(line_text, anchor_id))?;
            Some(PrepareRenameResult {
                placeholder: anchor_id.to_string(),
                line: node_info.line,
                start_col: start,
                end_col: end,
            })
        }
        NodeKind::AttributeRef { name } | NodeKind::AttributeDef { name, .. }
            if crate::completion::doc_attribute_doc(name).is_some() =>
        {
            None
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
        NodeKind::AttributeDef { name, .. } => {
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

/// An xref that names its target by title or reftext (`<<Command line>>`)
/// in every document that reads it, not by ID: renaming the ID would rewrite
/// the text into an ID reference.
fn names_no_id(catalogs: &CatalogCache<'_>, doc_path: &Path, target: &str, id: &str) -> bool {
    parse_xref_target(target).0.is_none()
        && catalogs
            .scopes_at(doc_path)
            .iter()
            .all(|scope| scope.lookup(id).is_none())
}

/// A name an attribute reference `{name}` can carry: a word character, then
/// word characters or `-` (Asciidoctor's `AttributeReferenceRx`).
fn is_valid_attribute_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '-'))
}

/// Why the symbol at `pos` cannot be renamed to `new_name`, if it cannot:
/// the name is no valid ID or attribute name, names a built-in attribute, or
/// the document already defines it (the rename would merge two symbols).
pub fn rename_conflict(
    content: &str,
    pos: Position,
    doc_path: &Path,
    workspace: Option<&Workspace>,
    new_name: &str,
) -> Option<String> {
    let node_info = find_node_at_position(content, pos)?;
    match &node_info.kind {
        NodeKind::Anchor { .. } | NodeKind::Xref { .. } | NodeKind::ShorthandXref { .. } => {
            if !is_valid_anchor_id(new_name) {
                return Some(format!("`{new_name}` is not a valid ID"));
            }
            let old_id = match &node_info.kind {
                NodeKind::Anchor { id } => Some(id.as_str()),
                NodeKind::Xref { target } | NodeKind::ShorthandXref { target } => {
                    parse_xref_target(target).1
                }
                _ => None,
            };
            let catalogs = CatalogCache::new(workspace?);
            (old_id != Some(new_name)
                && catalogs
                    .scopes_at(doc_path)
                    .iter()
                    .any(|scope| scope.lookup(new_name).is_some()))
            .then(|| format!("ID `{new_name}` is already defined"))
        }
        NodeKind::AttributeRef { name } | NodeKind::AttributeDef { name, .. } => {
            if !is_valid_attribute_name(new_name) {
                return Some(format!("`{new_name}` is not a valid attribute name"));
            }
            if crate::completion::doc_attribute_doc(new_name).is_some() {
                return Some(format!("`{new_name}` is a built-in attribute"));
            }
            let defined = workspace?.definition_sites(doc_path, node_info.line, new_name);
            (new_name != name && !defined.is_empty())
                .then(|| format!("attribute `{new_name}` is already defined"))
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
        NodeKind::AttributeRef { name } | NodeKind::AttributeDef { name, .. } => {
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
        NodeKind::Anchor { id } => {
            let catalogs = CatalogCache::new(ws);
            let definitions = anchor_definitions_of_anchor(ws, &catalogs, doc_path, id);
            resolve_rename_anchor(id, &definitions, &catalogs, overrides)
        }
        NodeKind::Xref { target } | NodeKind::ShorthandXref { target } => {
            let anchor_id = parse_xref_target(target).1?;
            let catalogs = CatalogCache::new(ws);
            if names_no_id(&catalogs, doc_path, target, anchor_id) {
                return None;
            }
            let definitions = anchor_definitions_of_xref(
                ws,
                &catalogs,
                doc_path,
                target,
                anchor_id,
                node_info.line,
            );
            resolve_rename_anchor(anchor_id, &definitions, &catalogs, overrides)
        }
        NodeKind::Include { .. } | NodeKind::Image { .. } => None,
    }
}

/// Rename all occurrences of an anchor: its `definitions` (the first one per
/// top-level document and the duplicates) and the xrefs that reach them.
fn resolve_rename_anchor(
    anchor_id: &str,
    definitions: &[AnchorLocation],
    catalogs: &CatalogCache<'_>,
    overrides: &HashMap<PathBuf, Arc<String>>,
) -> Option<RenameEdits> {
    let mut edits: HashMap<PathBuf, Vec<(usize, usize, usize)>> = HashMap::new();
    let mut file_cache: HashMap<PathBuf, String> = HashMap::new();

    for loc in definitions {
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

    // Anchor references (xrefs)
    for loc in anchor_reference_locations(catalogs, anchor_id, definitions, false, overrides) {
        edits
            .entry(loc.path)
            .or_default()
            .push((loc.line, loc.col, loc.end_col));
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

/// Find the byte offsets of `id` where a line defines it: `[[id]]` /
/// `[[id,reftext]]`, `anchor:id[...]`, the `#id` shorthand or `id=id` in an attribute list.
/// Returns `(start, end)` byte offsets of the id (exclusive end).
fn find_anchor_def_columns(line_text: &str, id: &str) -> Option<(usize, usize)> {
    let pattern = format!("[[{id}");
    let anchor = line_text.match_indices(&pattern).find_map(|(pos, _)| {
        let end = pos + pattern.len();
        matches!(line_text[end..].chars().next(), Some(']' | ',')).then_some((pos + 2, end))
        // skip `[[`
    });
    if anchor.is_some() {
        return anchor;
    }
    if let Some(pos) = line_text.find(&format!("anchor:{id}[")) {
        let start = pos + "anchor:".len();
        return Some((start, start + id.len()));
    }
    if !line_text.starts_with('[') {
        return None;
    }
    let ends_id = |end: usize| {
        matches!(
            line_text.as_bytes().get(end),
            Some(b'.' | b'%' | b'#' | b',' | b']')
        )
    };
    let shorthand = format!("#{id}");
    let mut from = 0;
    while let Some(rel) = line_text[from..].find(&shorthand) {
        let start = from + rel + 1;
        if ends_id(start + id.len()) && !line_text[..start].contains('"') {
            return Some((start, start + id.len()));
        }
        from = start;
    }
    for value in [format!("id={id}"), format!("id=\"{id}\"")] {
        let mut from = 0;
        while let Some(rel) = line_text[from..].find(&value) {
            let pos = from + rel;
            let quoted = value.ends_with('"');
            let start = pos + 3 + usize::from(quoted);
            let preceded = line_text[..pos].ends_with(['[', ',', ' ']);
            let end = start + id.len();
            let terminated = quoted || matches!(line_text.as_bytes().get(end), Some(b',' | b']'));
            if preceded && terminated {
                return Some((start, end));
            }
            from = pos + 1;
        }
    }
    None
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

/// Find the byte offsets of `name` inside `:name:`, `:name!:` or `:!name:` at
/// the start of a line. Returns `(start, end)` byte offsets of the name
/// (exclusive end).
fn find_attr_def_columns(line_text: &str, name: &str) -> Option<(usize, usize)> {
    [
        (format!(":{name}:"), 1),
        (format!(":{name}!:"), 1),
        (format!(":!{name}:"), 2),
    ]
    .into_iter()
    .find(|(pattern, _)| line_text.starts_with(pattern))
    .map(|(_, start)| (start, start + name.len()))
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

    // --- xrefs resolve per top-level document (fixture `anchor_scope`) ---

    fn anchor_scope_fixture() -> (PathBuf, Workspace) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/anchor_scope")
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();
        (root, ws)
    }

    fn goto_in_fixture(file: &str, line: u32, character: u32) -> (PathBuf, Vec<(PathBuf, usize)>) {
        let (root, ws) = anchor_scope_fixture();
        let path = root.join(file);
        let content = fs::read_to_string(&path).unwrap();
        let targets =
            resolve_goto_definitions(&content, Position { line, character }, &path, Some(&ws));
        (root, targets)
    }

    /// Asciidoctor (`natural_xrefs/main.adoc`): `<<Installation>>` renders as
    /// `#_installation` (section title), `<<Command line>>` as `#cli`
    /// (reftext of the anchor above `== CLI usage`).
    #[test]
    fn test_goto_def_natural_xref_lands_on_title_or_reftext_owner() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/natural_xrefs")
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        let path = root.join("main.adoc");
        let content = fs::read_to_string(&path).unwrap();
        let goto = |character| {
            resolve_goto_definitions(
                &content,
                Position {
                    line: 19,
                    character,
                },
                &path,
                Some(&ws),
            )
        };

        assert_eq!(goto(13), vec![(path.clone(), 9)]);
        assert_eq!(goto(42), vec![(path.clone(), 13)]);
    }

    /// A natural xref shows its target as an xref to the ID it resolves to
    /// does: the title of the section carrying the ID, else the anchor.
    /// Asciidoctor (`natural_xrefs/main.adoc`): `<<Command line>>` renders
    /// `href="#cli"`, `<<Free text>>` `href="#free"`, `<<Shared text>>`
    /// `href="#first-owner"`.
    #[test]
    fn test_hover_natural_xref_shows_the_id_it_resolves_to() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/natural_xrefs")
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        let path = root.join("main.adoc");
        let content = fs::read_to_string(&path).unwrap();
        let hover = |line, character| {
            resolve_hover(&content, Position { line, character }, &path, Some(&ws)).unwrap()
        };

        assert_eq!(hover(19, 13), "\u{2192} Installation\n\nmain.adoc:9");
        assert_eq!(hover(19, 42), "\u{2192} CLI usage\n\nmain.adoc:13");
        assert_eq!(hover(25, 26), "[[free]]\n\nmain.adoc:26");
        assert_eq!(hover(40, 38), "[[first-owner]]\n\nmain.adoc:35");
    }

    /// Asciidoctor (`xref_path_in_include/index.adoc`) reads the file part of
    /// an xref in the included `chapters/setup.adoc` from the folder of
    /// `index.adoc`: `xref:chapters/reference.adoc#ref-options[]` renders as
    /// `#ref-options`, `xref:reference.adoc#ref-options[]` as
    /// `reference.html#ref-options`, a link to the top-level `reference.adoc`,
    /// which has no `ref-options`. The preview reads `setup.adoc` alone, from
    /// its own folder, where the second path names `chapters/reference.adoc`.
    #[test]
    fn test_xref_path_in_included_file_resolves_from_the_top_level_document() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/xref_path_in_include")
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        let path = root.join("chapters/setup.adoc");
        let content = fs::read_to_string(&path).unwrap();
        let at = |line, character| Position { line, character };

        let targets = resolve_goto_definitions(&content, at(8, 40), &path, Some(&ws));
        assert_eq!(targets, vec![(root.join("chapters/reference.adoc"), 3)]);
        let hover = resolve_hover(&content, at(10, 55), &path, Some(&ws)).unwrap();
        assert_eq!(
            hover,
            "In index.adoc:\n\u{2717} Target not found\n\n\
             In setup.adoc (preview):\n\u{2192} Options\n\nreference.adoc:3"
        );
    }

    /// Goto, find-references and rename follow an xref only where the
    /// documents that read its file find the target, not in the preview of
    /// the file. Asciidoctor (`xref_path_in_include/index.adoc`):
    /// `xref:reference.adoc#ref-options[]` in `chapters/setup.adoc` is the
    /// dead link `reference.html#ref-options`, which the preview alone would
    /// find in `chapters/reference.adoc`;
    /// `xref:chapters/reference.adoc#ref-options[]` is `#ref-options`.
    #[test]
    fn test_xref_broken_in_the_master_is_not_followed_through_the_preview() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/xref_path_in_include")
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        let path = root.join("chapters/setup.adoc");
        let content = fs::read_to_string(&path).unwrap();
        let at = |line, character| Position { line, character };
        let none = HashMap::new();

        assert_eq!(
            resolve_goto_definitions(&content, at(10, 55), &path, Some(&ws)),
            vec![]
        );
        let references = resolve_references(&content, at(10, 55), &path, Some(&ws), true, &none);
        assert!(references.unwrap_or_default().is_empty());
        assert_eq!(prepare_rename(&content, at(10, 55), &path, Some(&ws)), None);

        assert_eq!(
            resolve_goto_definitions(&content, at(8, 40), &path, Some(&ws)),
            vec![(root.join("chapters/reference.adoc"), 3)]
        );
    }

    /// A file no document includes is its own document and keeps goto on a
    /// link to another document.
    #[test]
    fn test_goto_def_file_xref_in_standalone_file_keeps_its_target() {
        let (root, targets) = goto_in_fixture("solo/foreign.adoc", 2, 20);
        assert_eq!(targets, vec![(root.join("chapters/setup.adoc"), 4)]);
    }

    fn paths_in_include_preview() -> (PathBuf, Workspace) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/paths_in_include_preview")
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();
        (root, ws)
    }

    /// Goto follows an image only to the file the documents that read it
    /// find, not to the one only the preview finds; hover still shows the
    /// preview. Asciidoctor (`paths_in_include_preview/index.adoc`) renders
    /// `images/found.png` and `images/preview.png`, and only the first exists.
    #[test]
    fn test_image_found_only_by_the_preview_has_no_goto() {
        let (root, ws) = paths_in_include_preview();
        let path = root.join("chapters/chapter.adoc");
        let content = fs::read_to_string(&path).unwrap();
        let at = |line, character| Position { line, character };

        assert_eq!(
            resolve_goto_definitions(&content, at(9, 8), &path, Some(&ws)),
            vec![]
        );
        assert_eq!(
            resolve_goto_definitions(&content, at(11, 12), &path, Some(&ws)),
            vec![]
        );
        assert_eq!(
            resolve_hover(&content, at(9, 8), &path, Some(&ws)).unwrap(),
            "In index.adoc:\nimage::images/preview.png[]\n\n\u{2717} File not found\n\n\
             In chapter.adoc (preview):\nimage::preview.png[]\n\n\u{2713} File exists"
        );

        assert_eq!(
            resolve_goto_definitions(&content, at(7, 8), &path, Some(&ws)),
            vec![(root.join("images/found.png"), 1)]
        );
    }

    /// Goto and find-references follow an include only to the file the
    /// documents that read it find, as the diagnostic does; hover still
    /// shows the preview. Asciidoctor (`paths_in_include_preview/index.adoc`):
    /// `include file not found: .../parts/preview.adoc` for line 16;
    /// `shared/found.adoc` is included.
    #[test]
    fn test_include_found_only_by_the_preview_has_no_goto() {
        let (root, ws) = paths_in_include_preview();
        let path = root.join("chapters/chapter.adoc");
        let content = fs::read_to_string(&path).unwrap();
        let at = |line, character| Position { line, character };
        let none = HashMap::new();

        assert!(ws
            .diagnostics_for(&path)
            .iter()
            .any(|d| d.line == 16 && matches!(&d.kind, DiagnosticKind::BrokenInclude { .. })));
        assert_eq!(
            resolve_goto_definitions(&content, at(15, 25), &path, Some(&ws)),
            vec![]
        );
        let references = resolve_references(&content, at(15, 25), &path, Some(&ws), true, &none);
        assert!(references.unwrap_or_default().is_empty());
        let hover = resolve_hover(&content, at(15, 25), &path, Some(&ws)).unwrap();
        assert!(
            hover.ends_with(&format!(
                "In chapter.adoc (preview):\ninclude::{}/parts/preview.adoc[]\n\n\
                 \u{2713} File exists | 0 sections",
                root.join("chapters").display()
            )),
            "{hover}"
        );

        assert_eq!(
            resolve_goto_definitions(&content, at(13, 25), &path, Some(&ws)),
            vec![(root.join("shared/found.adoc"), 1)]
        );
    }

    /// A file no document includes is its own document: its image and
    /// include keep their goto. Asciidoctor (`chapters/solo.adoc`) renders
    /// `preview.png` next to it and includes `parts/preview.adoc`.
    #[test]
    fn test_image_and_include_in_standalone_file_keep_their_goto() {
        let (root, ws) = paths_in_include_preview();
        let path = root.join("chapters/solo.adoc");
        let content = fs::read_to_string(&path).unwrap();
        let at = |line, character| Position { line, character };

        assert_eq!(
            resolve_goto_definitions(&content, at(4, 8), &path, Some(&ws)),
            vec![(root.join("chapters/preview.png"), 1)]
        );
        assert_eq!(
            resolve_goto_definitions(&content, at(6, 25), &path, Some(&ws)),
            vec![(root.join("chapters/parts/preview.adoc"), 1)]
        );
    }

    /// Asciidoctor resolves `<<_second>>` in the document it converts: each
    /// of the two unrelated documents has its own `_second` and `_first`.
    /// Both directions are asked, so a global first-wins cannot pass by
    /// luck.
    #[test]
    fn test_goto_def_bare_xref_stays_in_its_own_top_level_document() {
        let (root, targets) = goto_in_fixture("solo/main.adoc", 6, 12);
        assert_eq!(targets, vec![(root.join("solo/main.adoc"), 5)]);

        let (root, targets) = goto_in_fixture("solo2/main.adoc", 4, 12);
        assert_eq!(targets, vec![(root.join("solo2/main.adoc"), 3)]);
    }

    /// Asciidoctor (`book.adoc`): the xref names `reference.adoc`, which
    /// `book.adoc` includes, so it is internal and `setup-only` is found in
    /// `setup.adoc`, wherever it is defined in the document.
    #[test]
    fn test_goto_def_file_xref_into_included_file_finds_id_in_whole_document() {
        let (root, targets) = goto_in_fixture("book.adoc", 6, 30);
        assert_eq!(targets, vec![(root.join("chapters/setup.adoc"), 4)]);
    }

    /// `reference.adoc` is part of `book.adoc` and `web.adoc`; `setup-only`
    /// exists in the first only, so only it offers a target.
    #[test]
    fn test_goto_def_bare_xref_in_shared_file_follows_the_documents_that_define_it() {
        let (root, targets) = goto_in_fixture("chapters/reference.adoc", 5, 8);
        assert_eq!(targets, vec![(root.join("chapters/setup.adoc"), 4)]);
    }

    /// Asciidoctor (`book.adoc`): `xref:chapters/reference.adoc#shared[]`
    /// renders `href="#shared"`, the first `shared` of the document, which is
    /// the one in `setup.adoc`, although `reference.adoc` defines it too. The
    /// named file does not narrow the search. (`shared` is also a duplicate,
    /// which Asciidoctor warns about.)
    #[test]
    fn test_goto_def_file_xref_prefers_first_definition_in_document_over_the_named_file() {
        let (root, targets) = goto_in_fixture("book.adoc", 8, 12);
        assert_eq!(targets, vec![(root.join("chapters/setup.adoc"), 1)]);
    }

    /// `setup.adoc` and `reference.adoc` belong to no document that
    /// `solo/foreign.adoc` is part of: Asciidoctor renders links
    /// (`../chapters/reference.html#shared`, `../chapters/setup.html#shared`)
    /// to the documents rendered from those files, each with its own
    /// `shared`. Both define it, so a first-wins index over all files would
    /// send one of the two to the wrong file.
    #[test]
    fn test_goto_def_file_xref_into_foreign_document_uses_that_files_ids() {
        let (root, targets) = goto_in_fixture("solo/foreign.adoc", 4, 20);
        assert_eq!(targets, vec![(root.join("chapters/reference.adoc"), 1)]);

        let (root, targets) = goto_in_fixture("solo/foreign.adoc", 4, 87);
        assert_eq!(targets, vec![(root.join("chapters/setup.adoc"), 1)]);
    }

    /// Asciidoctor: `web.adoc` gives `INFO: possible invalid reference:
    /// setup-only` (it includes `reference.adoc` without `setup.adoc`),
    /// `book.adoc` gives nothing. Hover names each document and what it finds.
    #[test]
    fn test_hover_bare_xref_in_shared_file_shows_each_document_that_differs() {
        let (root, ws) = anchor_scope_fixture();
        let path = root.join("chapters/reference.adoc");
        let content = fs::read_to_string(&path).unwrap();
        let pos = Position {
            line: 5,
            character: 8,
        };

        let hover = resolve_hover(&content, pos, &path, Some(&ws)).unwrap();

        assert!(hover.contains("In book.adoc:"), "got: {hover}");
        assert!(hover.contains("setup.adoc:4"), "got: {hover}");
        assert!(
            hover.contains("In web.adoc:\n\u{2717} Target not found"),
            "got: {hover}"
        );
    }

    /// Asciidoctor: no message for `web3.adoc`, which reads `reference.adoc`
    /// with `tag=top`, where `<<setup-only>>` does not lie. The LSP knows no
    /// tag regions, so a document that reads the file only in part shows no
    /// "not found": hover names `book.adoc` alone.
    #[test]
    fn test_hover_xref_in_file_read_only_in_part_by_a_document_shows_no_missing_target() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/anchor_region_xref")
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();
        let path = root.join("chapters/reference.adoc");
        let content = fs::read_to_string(&path).unwrap();
        let pos = Position {
            line: 5,
            character: 8,
        };

        let hover = resolve_hover(&content, pos, &path, Some(&ws)).unwrap();

        assert!(hover.contains("setup.adoc:1"), "got: {hover}");
        assert!(!hover.contains("web3.adoc"), "got: {hover}");
        assert!(!hover.contains("not found"), "got: {hover}");
    }

    /// Asciidoctor (`partial/book.adoc`, `a.adoc` included with `lines=1..2`,
    /// `b.adoc` in full): `xref:b.adoc#a-id[]` renders `href="#a-id"`, so
    /// `b.adoc` is part of the document and the ID is found in `a.adoc`.
    /// `xref:a.adoc#b-id[]` renders `href="a.html#b-id"`: a partially
    /// included file is not part of the document, the xref is a link.
    #[test]
    fn test_goto_def_file_xref_into_fully_included_file_is_internal() {
        let (root, targets) = goto_in_fixture("partial/book.adoc", 6, 12);
        assert_eq!(targets, vec![(root.join("partial/a.adoc"), 1)]);
    }

    /// Same fixture: the link goes to the document rendered from `a.adoc`,
    /// which does not define `b-id`, so it does not jump into `b.adoc`.
    #[test]
    fn test_goto_def_file_xref_into_partially_included_file_is_a_link_not_internal() {
        let (_root, targets) = goto_in_fixture("partial/book.adoc", 6, 35);
        assert_eq!(targets, vec![]);
    }

    // --- one catalog build per document and request (fixture `anchor_hot`) ---

    /// `anchor_hot`: `ch1.adoc` defines `hot`; each of the three chapters has
    /// ten `<<hot>>` and ten `xref:ch1.adoc#hot[]`, so 60 xrefs. `book.adoc`
    /// includes all three, `web.adoc` only `ch2.adoc`.
    fn hot_fixture() -> (PathBuf, Workspace) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/anchor_hot")
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();
        (root, ws)
    }

    /// The documents of the fixture: no request needs more catalogs than
    /// there are documents, each read once.
    const HOT_DOCUMENTS: usize = 5;

    #[derive(Clone, Copy, Debug)]
    enum Request {
        References,
        PrepareRename,
        Rename,
    }

    /// Runs `request` on the cursor at (`line`, `character`) of `file` and
    /// returns the number of catalogs it read and the number of locations or
    /// edits it found.
    fn run_hot_request(request: Request, file: &str, line: u32, character: u32) -> (usize, usize) {
        let (root, ws) = hot_fixture();
        let path = root.join(file);
        let content = fs::read_to_string(&path).unwrap();
        let pos = Position { line, character };
        let before = ws.catalog_builds();
        let found = match request {
            Request::References => {
                resolve_references(&content, pos, &path, Some(&ws), true, &HashMap::new())
                    .map_or(0, |locations| locations.len())
            }
            Request::PrepareRename => {
                usize::from(prepare_rename(&content, pos, &path, Some(&ws)).is_some())
            }
            Request::Rename => resolve_rename(&content, pos, &path, Some(&ws), &HashMap::new())
                .map_or(0, |edits| edits.values().map(Vec::len).sum()),
        };
        (ws.catalog_builds() - before, found)
    }

    /// A request rebuilt catalogs per xref (1200 xrefs took 8 s). Now each
    /// catalog is read once per request, wherever the cursor stands.
    #[rstest]
    #[case::references_from_definition(Request::References, "ch1.adoc", 0, 3)]
    #[case::references_from_xref(Request::References, "ch2.adoc", 2, 4)]
    #[case::references_from_file_xref(Request::References, "ch2.adoc", 13, 18)]
    #[case::prepare_rename_from_definition(Request::PrepareRename, "ch1.adoc", 0, 3)]
    #[case::prepare_rename_from_xref(Request::PrepareRename, "ch2.adoc", 2, 4)]
    #[case::rename_from_definition(Request::Rename, "ch1.adoc", 0, 3)]
    #[case::rename_from_xref(Request::Rename, "ch2.adoc", 2, 4)]
    fn test_request_reads_each_catalog_at_most_once(
        #[case] request: Request,
        #[case] file: &str,
        #[case] line: u32,
        #[case] character: u32,
    ) {
        let (builds, _) = run_hot_request(request, file, line, character);

        assert!(
            builds <= HOT_DOCUMENTS,
            "{request:?} read {builds} catalogs, the fixture has {HOT_DOCUMENTS} documents"
        );
    }

    /// Pins the results the cache must not change: 60 xrefs and the
    /// definition.
    #[rstest]
    #[case::references_from_definition(Request::References, "ch1.adoc", 0, 3, 61)]
    #[case::references_from_xref(Request::References, "ch2.adoc", 2, 4, 61)]
    #[case::rename_from_definition(Request::Rename, "ch1.adoc", 0, 3, 61)]
    #[case::rename_from_xref(Request::Rename, "ch2.adoc", 2, 4, 61)]
    #[case::prepare_rename_from_definition(Request::PrepareRename, "ch1.adoc", 0, 3, 1)]
    fn test_request_on_hot_id_finds_every_xref(
        #[case] request: Request,
        #[case] file: &str,
        #[case] line: u32,
        #[case] character: u32,
        #[case] expected: usize,
    ) {
        let (_, found) = run_hot_request(request, file, line, character);

        assert_eq!(found, expected);
    }

    // --- references, rename and explain per top-level document ---

    /// The files a rename edits, as paths relative to the fixture, with the
    /// edited lines.
    fn rename_in_fixture(file: &str, line: u32, character: u32) -> Vec<(String, Vec<usize>)> {
        let (root, ws) = anchor_scope_fixture();
        let path = root.join(file);
        let content = fs::read_to_string(&path).unwrap();
        let edits = resolve_rename(
            &content,
            Position { line, character },
            &path,
            Some(&ws),
            &HashMap::new(),
        )
        .unwrap();
        let mut files: Vec<(String, Vec<usize>)> = edits
            .into_iter()
            .map(|(file, edits)| {
                let mut lines: Vec<usize> = edits.iter().map(|(line, _, _)| *line).collect();
                lines.sort_unstable();
                (
                    file.strip_prefix(&root).unwrap().display().to_string(),
                    lines,
                )
            })
            .collect();
        files.sort();
        files
    }

    fn references_in_fixture(
        file: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Vec<(String, usize)> {
        let (root, ws) = anchor_scope_fixture();
        let path = root.join(file);
        let content = fs::read_to_string(&path).unwrap();
        resolve_references(
            &content,
            Position { line, character },
            &path,
            Some(&ws),
            include_declaration,
            &HashMap::new(),
        )
        .unwrap()
        .into_iter()
        .map(|loc| {
            (
                loc.path.strip_prefix(&root).unwrap().display().to_string(),
                loc.line,
            )
        })
        .collect()
    }

    /// `solo/main.adoc` and `solo2/main.adoc` are two documents that both
    /// define `note`. Asciidoctor knows each only in its own document, so
    /// renaming one leaves the other alone, whichever of the two is renamed.
    #[test]
    fn test_rename_anchor_stays_in_its_own_top_level_document() {
        assert_eq!(
            rename_in_fixture("solo/main.adoc", 8, 4),
            vec![("solo/main.adoc".to_string(), vec![9, 10])]
        );
        assert_eq!(
            rename_in_fixture("solo2/main.adoc", 8, 4),
            vec![("solo2/main.adoc".to_string(), vec![9, 10])]
        );
    }

    /// Rename started on a reference finds the definition of its own
    /// document, not the other document's.
    #[test]
    fn test_rename_from_xref_stays_in_its_own_top_level_document() {
        assert_eq!(
            rename_in_fixture("solo2/main.adoc", 9, 22),
            vec![("solo2/main.adoc".to_string(), vec![9, 10])]
        );
        assert_eq!(
            rename_in_fixture("solo/main.adoc", 9, 22),
            vec![("solo/main.adoc".to_string(), vec![9, 10])]
        );
    }

    #[test]
    fn test_references_stay_in_their_own_top_level_document() {
        assert_eq!(
            references_in_fixture("solo/main.adoc", 8, 4, true),
            vec![
                ("solo/main.adoc".to_string(), 9),
                ("solo/main.adoc".to_string(), 10)
            ]
        );
        assert_eq!(
            references_in_fixture("solo2/main.adoc", 9, 22, false),
            vec![("solo2/main.adoc".to_string(), 10)]
        );
    }

    /// `book.adoc` reads `shared` in `setup.adoc` first and again in
    /// `reference.adoc` (a duplicate): renaming from the xref renames both
    /// definitions and every xref that reaches either (the links in
    /// `solo/foreign.adoc` to the chapters rendered on their own among them),
    /// and leaves `standalone.adoc`, a document of its own, alone.
    #[test]
    fn test_rename_shared_id_renames_the_documents_definitions_only() {
        assert_eq!(
            rename_in_fixture("book.adoc", 6, 10),
            vec![
                ("book.adoc".to_string(), vec![7, 9]),
                ("chapters/reference.adoc".to_string(), vec![1]),
                ("chapters/setup.adoc".to_string(), vec![1]),
                // links to the documents rendered from the two chapters, one
                // edit per xref
                ("solo/foreign.adoc".to_string(), vec![5, 5]),
                ("solo/mixed.adoc".to_string(), vec![3]),
            ]
        );
    }

    /// The columns a rename edits in `file`, in order, for a rename started
    /// in `start_file` at `line`, `character`.
    fn rename_columns_in_fixture(
        start_file: &str,
        line: u32,
        character: u32,
        file: &str,
    ) -> Vec<(usize, usize, usize)> {
        let (root, ws) = anchor_scope_fixture();
        let path = root.join(start_file);
        let content = fs::read_to_string(&path).unwrap();
        let mut edits = resolve_rename(
            &content,
            Position { line, character },
            &path,
            Some(&ws),
            &HashMap::new(),
        )
        .unwrap()
        .remove(&root.join(file))
        .unwrap_or_default();
        edits.sort_unstable();
        edits
    }

    /// The byte column of the `n`-th `#shared` in line `line` (1-based) of a
    /// fixture file, plus one: the start of the ID.
    fn shared_column(file: &str, line: usize, n: usize) -> usize {
        let (root, _) = anchor_scope_fixture();
        let text = fs::read_to_string(root.join(file)).unwrap();
        let text = text.lines().nth(line - 1).unwrap().to_string();
        text.match_indices("#shared").nth(n).unwrap().0 + 1
    }

    /// Both xrefs of `solo/foreign.adoc:5` reach a `shared` definition of the
    /// document (the chapters rendered on their own), so both are renamed.
    #[test]
    fn test_rename_edits_every_reaching_xref_on_one_line() {
        let first = shared_column("solo/foreign.adoc", 5, 0);
        let second = shared_column("solo/foreign.adoc", 5, 1);
        assert_eq!(
            rename_columns_in_fixture("book.adoc", 6, 10, "solo/foreign.adoc"),
            vec![(5, first, first + 6), (5, second, second + 6)]
        );
    }

    /// `solo/mixed.adoc:3` has an xref to the `shared` of `standalone.adoc`
    /// (another document's ID, left alone) before one to `setup.adoc`'s:
    /// only the second is renamed.
    #[test]
    fn test_rename_skips_an_xref_on_the_same_line_that_reaches_another_document() {
        let setup = shared_column("solo/mixed.adoc", 3, 1);
        assert_eq!(
            rename_columns_in_fixture("book.adoc", 6, 10, "solo/mixed.adoc"),
            vec![(3, setup, setup + 6)]
        );
    }

    /// An xref without a definition has nothing to rename, so the client is
    /// not offered a rename that returns no edits.
    #[test]
    fn test_prepare_rename_rejects_an_xref_without_definition() {
        let (root, ws) = anchor_scope_fixture();
        let path = root.join("solo/foreign.adoc");
        let content = fs::read_to_string(&path).unwrap();
        let col = content.lines().nth(2).unwrap().find("#nope").unwrap() + 2;
        let pos = Position {
            line: 2,
            character: u32::try_from(col).unwrap(),
        };
        assert!(prepare_rename(&content, pos, &path, Some(&ws)).is_none());
    }

    // --- rename and references reach every definition an xref reaches ---

    fn anchor_rename_fixture() -> (PathBuf, Workspace) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/anchor_rename")
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();
        (root, ws)
    }

    /// The lines a rename edits, per file relative to the fixture, and the
    /// lines find-references lists with the declaration, for a request at
    /// `line`, `character` of `file`.
    fn rename_and_references_in_anchor_rename(
        file: &str,
        line: u32,
        character: u32,
    ) -> (Vec<(String, Vec<usize>)>, Vec<(String, usize)>) {
        let (root, ws) = anchor_rename_fixture();
        let path = root.join(file);
        let content = fs::read_to_string(&path).unwrap();
        let pos = Position { line, character };
        let relative = |path: &Path| path.strip_prefix(&root).unwrap().display().to_string();
        let mut renamed: Vec<(String, Vec<usize>)> =
            resolve_rename(&content, pos, &path, Some(&ws), &HashMap::new())
                .unwrap()
                .into_iter()
                .map(|(file, edits)| {
                    let mut lines: Vec<usize> = edits.iter().map(|(line, _, _)| *line).collect();
                    lines.sort_unstable();
                    (relative(&file), lines)
                })
                .collect();
        renamed.sort();
        let references = resolve_references(&content, pos, &path, Some(&ws), true, &HashMap::new())
            .unwrap()
            .into_iter()
            .map(|loc| (relative(&loc.path), loc.line))
            .collect();
        (renamed, references)
    }

    /// `book1.adoc` and `book2.adoc` each define `id` in their own chapter
    /// and share `c.adoc` with `<<id>>`: Asciidoctor resolves that xref in
    /// both books. A rename (and the references) must not depend on where
    /// it starts: both definitions and the shared xref, or book2 breaks.
    #[test]
    fn test_rename_and_references_cover_both_definitions_of_a_shared_xref() {
        let renamed = vec![
            ("a.adoc".to_string(), vec![1]),
            ("b.adoc".to_string(), vec![1]),
            ("c.adoc".to_string(), vec![3]),
        ];
        let referenced = vec![
            ("a.adoc".to_string(), 1),
            ("b.adoc".to_string(), 1),
            ("c.adoc".to_string(), 3),
        ];
        for (file, line, character) in [("a.adoc", 0, 3), ("b.adoc", 0, 3), ("c.adoc", 2, 7)] {
            let (edits, references) = rename_and_references_in_anchor_rename(file, line, character);
            assert_eq!(edits, renamed, "rename from {file}");
            assert_eq!(references, referenced, "references from {file}");
        }
    }

    /// `foreign.adoc` links `x.adoc#dup`; `x.adoc` includes `y.adoc` and
    /// `z.adoc`, which both define `dup`. The link resolves to `y.adoc` with
    /// `z.adoc` as the duplicate: a rename from the link edits the same set
    /// as one from the definition.
    #[test]
    fn test_rename_from_a_foreign_xref_renames_the_duplicates_of_its_document() {
        let expected = vec![
            ("foreign.adoc".to_string(), vec![3]),
            ("y.adoc".to_string(), vec![1]),
            ("z.adoc".to_string(), vec![1]),
        ];
        for (file, line, character) in [("foreign.adoc", 2, 17), ("y.adoc", 0, 3), ("z.adoc", 0, 3)]
        {
            let (edits, _) = rename_and_references_in_anchor_rename(file, line, character);
            assert_eq!(edits, expected, "rename from {file}");
        }
    }

    /// `solo/mixed.adoc:3` has two xrefs on one line: the prepare range of
    /// the second (`setup.adoc#shared`) must hold the cursor, not the first
    /// one's (`standalone.adoc#shared`).
    #[test]
    fn test_prepare_rename_range_is_the_xref_under_the_cursor() {
        let (root, ws) = anchor_scope_fixture();
        let path = root.join("solo/mixed.adoc");
        let content = fs::read_to_string(&path).unwrap();
        let second = shared_column("solo/mixed.adoc", 3, 1);
        let pos = Position {
            line: 2,
            character: u32::try_from(second + 2).unwrap(),
        };
        let range = prepare_rename(&content, pos, &path, Some(&ws)).unwrap();
        assert_eq!((range.start_col, range.end_col), (second, second + 6));
    }

    /// `reference.adoc` is part of `book.adoc` and `web.adoc`: its `shared`
    /// is the first one in `web.adoc`, so the report has one definition per
    /// document.
    #[test]
    fn test_explain_anchor_reports_the_definition_per_top_level_document() {
        let (root, ws) = anchor_scope_fixture();
        let path = root.join("chapters/reference.adoc");
        let content = fs::read_to_string(&path).unwrap();
        let pos = Position {
            line: 0,
            character: 4,
        };

        let report = explain_position(&content, pos, &path, &path, Some(&ws));

        let scopes = report["resolution"]["scopes"].as_array().unwrap();
        let found: Vec<(String, String, u64)> = scopes
            .iter()
            .map(|scope| {
                let definition = &scope["definition"];
                (
                    scope["scope"].as_str().unwrap().to_string(),
                    definition["file"].as_str().unwrap().to_string(),
                    definition["line"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            found,
            vec![
                (
                    "book.adoc".to_string(),
                    path_string(&root.join("chapters/setup.adoc")),
                    1
                ),
                ("web.adoc".to_string(), path_string(&path), 1),
            ]
        );
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

    // xrefs in admonitions, quotes, description lists and footnotes.
    #[rstest]
    #[case::admonition("TIP: See <<my-anchor>>.", 12)]
    #[case::quote_block("____\nSee <<my-anchor>>.\n____", 10)]
    #[case::markdown_quote("> See <<my-anchor>>.", 10)]
    #[case::dlist_inline("Term:: See <<my-anchor>>.", 13)]
    #[case::dlist_term("See <<my-anchor>>:: def", 7)]
    #[case::dlist_xref_macro("Term:: See xref:b.adoc#my-anchor[link].", 20)]
    #[case::footnote("Text.footnote:[See <<my-anchor>>.]", 22)]
    #[case::footnote_with_id("Text.footnote:fid[See <<my-anchor>>.]", 25)]
    #[case::footnote_xref_macro("Text.footnote:[See xref:b.adoc#my-anchor[link\\].]", 30)]
    fn test_goto_def_xref_in_block_context(#[case] body: &str, #[case] character: u32) {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        let b = root.join("b.adoc");
        let content = format!("= Doc A\n\n{body}\n\ninclude::b.adoc[]\n");
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

    /// `Gadget:: ... xref:chapters/reference.adoc#ref-options[Options]` in the
    /// committed fixture is a description list definition.
    #[test]
    fn test_goto_def_xref_in_fixture_description_list() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/adversarial_project")
            .canonicalize()
            .unwrap();
        let index = root.join("index.adoc");
        let content = fs::read_to_string(&index).unwrap();
        let ws = Workspace::build(&root, &index);
        let (row, text) = content
            .lines()
            .enumerate()
            .find(|(_, l)| l.starts_with("Gadget::"))
            .unwrap();
        let pos = Position {
            line: u32::try_from(row).unwrap(),
            character: u32::try_from(text.find("ref-options").unwrap()).unwrap(),
        };

        assert_eq!(
            resolve_goto_definitions(&content, pos, &index, Some(&ws)),
            vec![(root.join("chapters/reference.adoc"), 5)]
        );
    }

    /// Hover on the same `Gadget::` xref shows the target section.
    #[test]
    fn test_hover_xref_in_fixture_description_list() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/adversarial_project")
            .canonicalize()
            .unwrap();
        let index = root.join("index.adoc");
        let content = fs::read_to_string(&index).unwrap();
        let ws = Workspace::build(&root, &index);
        let (row, text) = content
            .lines()
            .enumerate()
            .find(|(_, l)| l.starts_with("Gadget::"))
            .unwrap();
        let pos = Position {
            line: u32::try_from(row).unwrap(),
            character: u32::try_from(text.find("ref-options").unwrap()).unwrap(),
        };

        let hover = resolve_hover(&content, pos, &index, Some(&ws)).unwrap();

        assert!(hover.contains("reference.adoc"), "got {hover}");
    }

    /// Hover at `needle` on the first line of `file` (below `root`) that
    /// contains it, in the workspace of the top-level document `index.adoc`.
    fn hover_in_fixture(root: &Path, file: &str, needle: &str) -> Option<String> {
        let path = root.join(file);
        let content = fs::read_to_string(&path).unwrap();
        let ws = Workspace::build(root, &root.join("index.adoc"));
        let (row, text) = content
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains(needle))
            .unwrap();
        let pos = Position {
            line: u32::try_from(row).unwrap(),
            character: u32::try_from(text.find(needle).unwrap()).unwrap(),
        };
        resolve_hover(&content, pos, &path, Some(&ws))
    }

    /// Hover on an anchor declaration shows what hover on an xref to it
    /// shows. Asciidoctor (`adversarial_project/index.adoc`): `<<glossary>>`
    /// renders as `<a href="#glossary">Glossary</a>` and `<<intro-audience,
    /// ...>>` links `#intro-audience`, the `Audience` section.
    #[rstest::rstest]
    #[case::block_anchor(
        ("index.adoc", "[[glossary]]"),
        ("chapters/reference.adoc", "<<glossary>>"),
        "\u{2192} Glossary\n\nindex.adoc:25"
    )]
    #[case::block_id(
        // A `[#id]` attribute list counts with the cursor on the ID only.
        ("chapters/intro.adoc", "intro-audience]"),
        ("chapters/setup.adoc", "<<intro-audience"),
        "\u{2192} Audience\n\nintro.adoc:9"
    )]
    fn test_hover_anchor_declaration_shows_what_an_xref_to_it_shows(
        #[case] declaration: (&str, &str),
        #[case] xref: (&str, &str),
        #[case] expected: &str,
    ) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/adversarial_project")
            .canonicalize()
            .unwrap();

        let on_declaration = hover_in_fixture(&root, declaration.0, declaration.1);
        let on_xref = hover_in_fixture(&root, xref.0, xref.1);

        assert_eq!(on_declaration.as_deref(), Some(expected));
        assert_eq!(on_declaration, on_xref);
    }

    /// Asciidoctor (`inline_anchor_macro/index.adoc`): `anchor:plain[]`
    /// renders `<a id="plain"></a>`, `<<plain>>` links it as `[plain]`. An
    /// anchor outside a section title shows the anchor and its place.
    #[test]
    fn test_hover_inline_anchor_macro_declaration_shows_what_an_xref_to_it_shows() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/inline_anchor_macro")
            .canonicalize()
            .unwrap();

        let on_declaration = hover_in_fixture(&root, "index.adoc", "anchor:plain[]");
        let on_xref = hover_in_fixture(&root, "index.adoc", "<<plain>>");

        assert_eq!(on_declaration.as_deref(), Some("[[plain]]\n\nindex.adoc:5"));
        assert_eq!(on_declaration, on_xref);
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
        let refs = resolve_references(chapter, pos, chapter_path, Some(&ws), true, &HashMap::new())
            .unwrap();
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

        // Line 46 (1-indexed): "...see xref:chapters/05_building_block_view.adoc#building-blocks[...]."
        let pos = Position {
            line: 45,
            character: 50,
        };
        let info = find_node_at_position(&content, pos);
        assert!(info.is_some(), "xref on line 46 must be found");
        assert_eq!(
            info.unwrap().kind,
            NodeKind::Xref {
                target: "chapters/05_building_block_view.adoc#building-blocks".into()
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

        // Line 68 (1-indexed): "...documented in xref:chapters/01_introduction_and_goals.adoc[...]..."
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
                target: "chapters/01_introduction_and_goals.adoc".into()
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
                name: "imagesdir".into(),
                unset: false
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

        let results =
            resolve_references(&content, pos, &a, Some(&ws), false, &HashMap::new()).unwrap();
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

        let results =
            resolve_references(&content, pos, &a, Some(&ws), false, &HashMap::new()).unwrap();
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
        let without =
            resolve_references(&content, pos, &a, Some(&ws), false, &HashMap::new()).unwrap();
        assert_eq!(without.len(), 1);

        // With includeDeclaration — usages + definition
        let with = resolve_references(&content, pos, &a, Some(&ws), true, &HashMap::new()).unwrap();
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
        let refs =
            resolve_references(&content, pos, &config, Some(&ws), true, &HashMap::new()).unwrap();
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

    /// References on an attribute entry `:min-ram:` list the entry itself with
    /// `includeDeclaration` (committed fixture `tests/fixtures/adversarial_project`).
    #[test]
    fn test_references_on_attribute_entry_include_declaration() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/adversarial_project")
            .canonicalize()
            .unwrap();
        let index = root.join("index.adoc");
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        let content = fs::read_to_string(&index).unwrap();
        // cursor on `:min-ram:` (index.adoc:10)
        let pos = Position {
            line: 9,
            character: 3,
        };

        let locations = |include_declaration| -> Vec<(PathBuf, usize)> {
            resolve_references(
                &content,
                pos,
                &index,
                Some(&ws),
                include_declaration,
                &HashMap::new(),
            )
            .unwrap()
            .iter()
            .map(|loc| (loc.path.clone(), loc.line))
            .collect()
        };
        let without = locations(false);
        let with = locations(true);

        assert_eq!(without.len(), 2, "usages only, got: {without:?}");
        assert!(!without.contains(&(index.clone(), 10)));
        assert_eq!(with.len(), 3, "usages + entry, got: {with:?}");
        assert!(with.contains(&(index, 10)), "{with:?}");
    }

    #[rstest]
    #[case::suffix_form(6, 3, vec![5], vec![2, 5, 7])]
    #[case::prefix_form(7, 3, vec![10], vec![3, 8, 10])]
    fn test_references_on_unset_attribute_entry(
        #[case] line: u32,
        #[case] character: u32,
        #[case] usages: Vec<usize>,
        #[case] with_entries: Vec<usize>,
    ) {
        let (index, content, ws) = unset_entry_fixture();
        let pos = Position { line, character };

        let lines = |include_declaration| -> Vec<usize> {
            let mut lines: Vec<usize> = resolve_references(
                &content,
                pos,
                &index,
                Some(&ws),
                include_declaration,
                &HashMap::new(),
            )
            .unwrap_or_default()
            .iter()
            .map(|loc| loc.line)
            .collect();
            lines.sort_unstable();
            lines
        };

        assert_eq!(lines(false), usages);
        assert_eq!(lines(true), with_entries);
    }

    #[rstest]
    #[case(":min-ram!:", "min-ram", Some((1, 8)))]
    #[case(":!min-ram:", "min-ram", Some((2, 9)))]
    #[case::suffix_with_value(":min-ram!: 4 GB", "min-ram", Some((1, 8)))]
    #[case::prefix_with_value(":!min-ram: 4 GB", "min-ram", Some((2, 9)))]
    #[case::both_bangs_rejected(":!min-ram!:", "min-ram", None)]
    #[case::trailing_garbage_rejected(":min-ram!x:", "min-ram", None)]
    #[case::other_name_rejected(":!other-ram: value", "min-ram", None)]
    fn test_find_attr_def_columns_unset_forms(
        #[case] line: &str,
        #[case] name: &str,
        #[case] expected: Option<(usize, usize)>,
    ) {
        assert_eq!(find_attr_def_columns(line, name), expected);
    }

    #[rstest]
    #[case::plain(":a: value", Some(("a", false)))]
    #[case::suffix_with_value(":a!: value", Some(("a", true)))]
    #[case::prefix_with_value(":!a: value", Some(("a", true)))]
    #[case::suffix_trailing_blanks(":d!:   ", Some(("d", true)))]
    #[case::bare_bang(":!:", None)]
    #[case::double_bang(":!!:", None)]
    #[case::both_bangs_not_recognised(":!a!:", None)]
    fn test_find_attribute_def_at_position_unset_forms(
        #[case] line: &str,
        #[case] expected: Option<(&str, bool)>,
    ) {
        let found = find_attribute_def_at_position(line, 1).map(|info| info.kind);
        let expected = expected.map(|(name, unset)| NodeKind::AttributeDef {
            name: name.to_string(),
            unset,
        });
        assert_eq!(found, expected);
    }

    #[test]
    fn test_hover_unset_entry_of_pdf_path_attribute_does_not_show_the_earlier_value() {
        let content = "= M\n:pdf-theme: custom\n:pdf-theme!:\n";
        let (_tmp, ws, paths) = scanned(&[("m.adoc", content)]);
        let pos = Position {
            line: 2,
            character: 1,
        };
        let hover = resolve_hover(content, pos, &paths[0], Some(&ws));
        assert_eq!(hover.as_deref(), Some(":pdf-theme!:"));
    }

    #[rstest]
    #[case::suffix_form(6, ":min-ram!:")]
    #[case::prefix_form_shown_as_suffix(7, ":other-ram!:")]
    fn test_hover_on_unset_entry_shows_the_unset_form(#[case] line: u32, #[case] expected: &str) {
        let (index, content, ws) = unset_entry_fixture();
        let pos = Position { line, character: 3 };
        let hover = resolve_hover(&content, pos, &index, Some(&ws));
        assert_eq!(hover.as_deref(), Some(expected));
    }

    #[test]
    fn test_hover_on_unset_entry_in_config_file_shows_the_unset_form() {
        let config = ":cfg!:\n";
        let (_tmp, ws, paths) = opened(&[(".asciidoctorconfig", config), ("a.adoc", "= A\n")]);
        let pos = Position {
            line: 0,
            character: 2,
        };
        let hover = resolve_hover(config, pos, &paths[0], Some(&ws));
        assert_eq!(hover.as_deref(), Some(":cfg!:"));
    }

    fn unset_entry_fixture() -> (PathBuf, String, Workspace) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/unset_entry_references")
            .canonicalize()
            .unwrap();
        let index = root.join("index.adoc");
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        let content = fs::read_to_string(&index).unwrap();
        (index, content, ws)
    }

    #[rstest]
    #[case::from_usage_of_suffix_unset(4, 8, vec![(2, 1, 8), (5, 7, 14), (7, 1, 8)])]
    #[case::from_usage_of_prefix_unset(9, 8, vec![(3, 1, 10), (8, 2, 11), (10, 7, 16)])]
    fn test_rename_edits_unset_entries(
        #[case] line: u32,
        #[case] character: u32,
        #[case] expected: Vec<(usize, usize, usize)>,
    ) {
        let (index, content, ws) = unset_entry_fixture();
        let pos = Position { line, character };
        let edits = resolve_rename(&content, pos, &index, Some(&ws), &HashMap::new()).unwrap();
        let mut found: Vec<_> = edits.into_values().flatten().collect();
        found.sort_unstable();
        assert_eq!(found, expected);
    }

    #[rstest]
    #[case::suffix_form(6, 3, 1, 8)]
    #[case::prefix_form(7, 3, 2, 11)]
    fn test_prepare_rename_on_unset_entry(
        #[case] line: u32,
        #[case] character: u32,
        #[case] start_col: usize,
        #[case] end_col: usize,
    ) {
        let (index, content, ws) = unset_entry_fixture();
        let pos = Position { line, character };
        let result = prepare_rename(&content, pos, &index, Some(&ws)).unwrap();
        assert_eq!(result.line, line as usize + 1);
        assert_eq!((result.start_col, result.end_col), (start_col, end_col));
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

        let results = resolve_references(
            &concepts_content,
            pos,
            &concepts_path,
            Some(&ws),
            false,
            &HashMap::new(),
        )
        .unwrap();
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

        let results = resolve_references(
            &config_content,
            pos,
            &config_path,
            Some(&ws),
            false,
            &HashMap::new(),
        )
        .unwrap();
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

        let results =
            resolve_references(&content, pos, &a, Some(&ws), false, &HashMap::new()).unwrap();
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

        let results =
            resolve_references(&content, pos, &a, Some(&ws), false, &HashMap::new()).unwrap();
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

        let results =
            resolve_references(&content, pos, &a, Some(&ws), false, &HashMap::new()).unwrap();
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
        let without =
            resolve_references(&content, pos, &a, Some(&ws), false, &HashMap::new()).unwrap();
        assert_eq!(without.len(), 1, "Expected 1 ref without declaration");

        // With includeDeclaration — should include anchor definition
        let with = resolve_references(&content, pos, &a, Some(&ws), true, &HashMap::new()).unwrap();
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

        let ws = Workspace::build(tmp.path(), &a);
        let content = fs::read_to_string(&b).unwrap();
        // cursor on xref:a.adoc#target[link]
        let pos = Position {
            line: 2,
            character: 8,
        };

        let results =
            resolve_references(&content, pos, &b, Some(&ws), false, &HashMap::new()).unwrap();
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

        let ws = Workspace::build(tmp.path(), &a);
        let content = fs::read_to_string(&b).unwrap();
        // cursor on <<anchor>>
        let pos = Position {
            line: 2,
            character: 7,
        };

        let results =
            resolve_references(&content, pos, &b, Some(&ws), false, &HashMap::new()).unwrap();
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

        let results =
            resolve_references(&content, pos, &index, Some(&ws), false, &HashMap::new()).unwrap();
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

        let results =
            resolve_references(&content, pos, &a, Some(&ws), false, &HashMap::new()).unwrap();
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
                name: "source-dir".into(),
                unset: false
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
                inline: false,
                attrlist: "Alt".into()
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
                inline: false,
                attrlist: String::new()
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
            ":imagesdir: {assets}/img\n\nResolves to doc/assets/img\n\nImages directory"
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
        assert_eq!(hover, ":imagesdir: {nowhere}/img\n\nImages directory");
    }

    const TOC_DOC: &str = "Table of contents placement\n\nValues: auto, left, right, preamble";

    fn hover_at(content: &str, line: u32, character: u32) -> Option<String> {
        let (_tmp, ws, paths) = scanned(&[("m.adoc", content)]);
        resolve_hover(content, Position { line, character }, &paths[0], Some(&ws))
    }

    /// Verify of the item: hover on `:toc:` shows description and values.
    #[test]
    fn test_hover_attribute_def_of_catalog_attribute_shows_description_and_values() {
        assert_eq!(
            hover_at("= M\n:toc: left\n", 1, 1).unwrap(),
            format!(":toc: left\n\n{TOC_DOC}")
        );
    }

    #[rstest]
    #[case(":sectnums:", ":sectnums:\n\nNumber sections")]
    #[case(":imagesdir: img", ":imagesdir: img\n\nImages directory")]
    fn test_hover_attribute_def_of_catalog_attribute_without_choices(
        #[case] entry: &str,
        #[case] expected: &str,
    ) {
        let content = format!("= M\n{entry}\n");
        assert_eq!(hover_at(&content, 1, 1).unwrap(), expected);
    }

    #[test]
    fn test_hover_attribute_def_catalog_name_is_case_insensitive() {
        assert_eq!(
            hover_at("= M\n:TOC: left\n", 1, 1).unwrap(),
            format!(":TOC: left\n\n{TOC_DOC}")
        );
    }

    #[rstest]
    #[case(":toc!:")]
    #[case(":!toc:")]
    fn test_hover_unset_entry_of_catalog_attribute_shows_its_description(#[case] unset: &str) {
        // Both spellings of the unset entry are shown in the `:name!:` form.
        let content = format!("= M\n:toc: left\n{unset}\n");
        assert_eq!(
            hover_at(&content, 2, 1).unwrap(),
            format!(":toc!:\n\n{TOC_DOC}")
        );
    }

    /// The description depends on the name only, so a file shared by two
    /// masters shows it once, after the last scope block.
    #[test]
    fn test_hover_attribute_def_in_shared_file_shows_catalog_doc_once() {
        let shared = ":imagesdir: {base}/img\n";
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
            "In m1.adoc:\n:imagesdir: {base}/img\n\nResolves to A/img\n\n\
             In m2.adoc:\n:imagesdir: {base}/img\n\nResolves to B/img\n\n\
             Images directory"
        );
    }

    /// Asciidoctor renders an entry-shaped line in a listing block as text
    /// (`<pre>`), so the description alone never creates a hover.
    #[test]
    fn test_hover_entry_line_in_listing_block_has_no_catalog_doc() {
        assert!(hover_at("= M\n\n----\n:toc: right\n----\n", 3, 1).is_none());
    }

    /// Verify of the item: hover on `:toc: left` (index.adoc:4) of the
    /// committed fixture.
    #[test]
    fn test_hover_toc_entry_in_adversarial_fixture() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/adversarial_project")
            .canonicalize()
            .unwrap();
        let index = root.join("index.adoc");
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        let content = fs::read_to_string(&index).unwrap();
        let pos = Position {
            line: 3,
            character: 2,
        };
        assert_eq!(
            resolve_hover(&content, pos, &index, Some(&ws)).unwrap(),
            format!(":toc: left\n\n{TOC_DOC}")
        );
    }

    #[test]
    fn test_hover_catalog_entry_in_adversarial_fixture() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/adversarial_project")
            .canonicalize()
            .unwrap();
        let index = root.join("index.adoc");
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        let content = fs::read_to_string(&index).unwrap();
        // cursor on `:icons: font` (index.adoc:7)
        let pos = Position {
            line: 6,
            character: 3,
        };
        assert_eq!(
            resolve_hover(&content, pos, &index, Some(&ws)).unwrap(),
            ":icons: font\n\nIcon mode\n\nValues: font, image"
        );
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
        assert_eq!(hover, "\u{2192} Chapter B\n\nb.adoc:1");
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
        assert_eq!(hover, "\u{2192} Chapter B\n\nb.adoc:1");
    }

    #[test]
    fn test_hover_broken_xref_says_target_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, "= Doc A\n\nSee <<nonexistent>>.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a);
        let content = fs::read_to_string(&a).unwrap();
        let pos = Position {
            line: 2,
            character: 8,
        };

        assert_eq!(
            resolve_hover(&content, pos, &a, Some(&ws)).as_deref(),
            Some("\u{2717} Target not found")
        );
    }

    #[rstest]
    #[case::missing_file("See xref:missing.adoc[].\n", 10)]
    #[case::missing_anchor_in_file("See xref:b.adoc#nope[].\n", 10)]
    #[case::mistyped_auto_id("See <<_b_typo>>.\n", 8)]
    fn test_hover_broken_file_xref_says_target_not_found(
        #[case] line: &str,
        #[case] character: u32,
    ) {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        let doc = format!("= Doc A\n\n{line}\ninclude::b.adoc[]\n");
        fs::write(&a, &doc).unwrap();
        fs::write(root.join("b.adoc"), "[[real]]\n== B\n").unwrap();
        let ws = Workspace::build(&root, &a);
        let pos = Position { line: 2, character };

        assert_eq!(
            resolve_hover(&doc, pos, &a, Some(&ws)).as_deref(),
            Some("\u{2717} Target not found")
        );
    }

    /// Asciidoctor: `[[first]]` followed by `[[second]]` registers only
    /// `second` (`possible invalid reference` for `first`).
    #[test]
    fn test_hover_xref_overridden_id_of_a_run_is_not_found() {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        let doc = "= Doc A\n\n[[first]]\n[[second]]\n== Run\n\nSee <<first>> and <<second>>.\n";
        fs::write(&a, doc).unwrap();
        let ws = Workspace::build(&root, &a);
        let at = |character| Position { line: 6, character };

        let first = resolve_hover(doc, at(8), &a, Some(&ws));
        let second = resolve_hover(doc, at(24), &a, Some(&ws));

        assert_eq!(first.as_deref(), Some("\u{2717} Target not found"));
        assert_eq!(second.as_deref(), Some("\u{2192} Run\n\na.adoc:4"));
    }

    #[rstest]
    #[case::block_anchor("[[intro]]\n== Introduction\n", "<<intro>>", "Introduction")]
    #[case::short_id("[#aud]\n== Audience\n", "<<aud>>", "Audience")]
    #[case::id_attribute("[id=by-attr]\n== B\n", "<<by-attr>>", "B")]
    #[case::inline_anchor("== Inline Title [[inl]]\n", "<<inl>>", "Inline Title")]
    #[case::auto_id("== Plain\n", "<<_plain>>", "Plain")]
    #[case::file_xref("[[intro]]\n== Introduction\n", "xref:b.adoc#intro[]", "Introduction")]
    fn test_hover_xref_shows_the_title_of_the_section_carrying_the_id(
        #[case] b_content: &str,
        #[case] xref: &str,
        #[case] title: &str,
    ) {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        let doc = format!("= Doc A\n\nSee {xref}.\n\ninclude::b.adoc[]\n");
        fs::write(&a, &doc).unwrap();
        fs::write(root.join("b.adoc"), b_content).unwrap();
        let ws = Workspace::build(&root, &a);
        let pos = Position {
            line: 2,
            character: 10,
        };

        let hover = resolve_hover(&doc, pos, &a, Some(&ws)).unwrap();

        assert!(
            hover.starts_with(&format!("\u{2192} {title}\n\nb.adoc:")),
            "got: {hover}"
        );
    }

    /// Asciidoctor renders `[p1]` for an anchor on a paragraph: the next
    /// section is not its title.
    #[test]
    fn test_hover_xref_to_a_paragraph_anchor_shows_the_anchor() {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        let doc = "= Doc A\n\nSee <<p1>>.\n\n[[p1]]\nA paragraph.\n\n== Later\n";
        fs::write(&a, doc).unwrap();
        let ws = Workspace::build(&root, &a);
        let pos = Position {
            line: 2,
            character: 8,
        };

        let hover = resolve_hover(doc, pos, &a, Some(&ws)).unwrap();

        assert!(hover.starts_with("[[p1]]"), "got: {hover}");
        assert!(!hover.contains("Later"), "got: {hover}");
    }

    #[test]
    fn test_hover_file_xref_shows_the_document_title() {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        let doc = "= Doc A\n\nSee xref:b.adoc[].\n\ninclude::b.adoc[]\n";
        fs::write(&a, doc).unwrap();
        fs::write(root.join("b.adoc"), "// c\n= Other Doc\n").unwrap();
        let ws = Workspace::build(&root, &a);
        let pos = Position {
            line: 2,
            character: 10,
        };

        let hover = resolve_hover(doc, pos, &a, Some(&ws)).unwrap();

        assert_eq!(hover, "\u{2192} Other Doc\n\nb.adoc:1");
    }

    #[test]
    fn test_hover_file_xref_without_a_document_title_shows_only_the_place() {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        let doc = "= Doc A\n\nSee xref:b.adoc[].\n\ninclude::b.adoc[]\n";
        fs::write(&a, doc).unwrap();
        fs::write(root.join("b.adoc"), "Text.\n").unwrap();
        let ws = Workspace::build(&root, &a);
        let pos = Position {
            line: 2,
            character: 10,
        };

        let hover = resolve_hover(doc, pos, &a, Some(&ws)).unwrap();

        assert_eq!(hover, "b.adoc:1");
    }

    /// Fixture review repro (G7): `[[intro]]` above `= Introduction`,
    /// included with `leveloffset=+1`.
    #[test]
    fn test_hover_xref_to_id_above_included_chapter_title_shows_title() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/adversarial_project")
            .canonicalize()
            .unwrap();
        let index = root.join("index.adoc");
        let content = fs::read_to_string(&index).unwrap();
        let ws = Workspace::build(&root, &index);
        let (row, text) = content
            .lines()
            .enumerate()
            .find(|(_, l)| l.starts_with("See <<intro>>"))
            .unwrap();
        let pos = Position {
            line: u32::try_from(row).unwrap(),
            character: u32::try_from(text.find("intro").unwrap()).unwrap(),
        };

        assert_eq!(
            resolve_hover(&content, pos, &index, Some(&ws)).as_deref(),
            Some("\u{2192} Introduction\n\nintro.adoc:1")
        );
    }

    /// Fixture `chapters/setup.adoc`: `:imagesdir:` is
    /// `{asciidoctorconfigdir}/images`, absolute; the hover shows it relative
    /// to the document folder.
    #[test]
    fn test_hover_image_with_absolute_imagesdir_shows_path_relative_to_docdir() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/adversarial_project")
            .canonicalize()
            .unwrap();
        let index = root.join("index.adoc");
        let setup = root.join("chapters/setup.adoc");
        let content = fs::read_to_string(&setup).unwrap();
        let ws = Workspace::build(&root, &index);
        let (row, text) = content
            .lines()
            .enumerate()
            .find(|(_, l)| l.starts_with("image::diagrams"))
            .unwrap();
        let pos = Position {
            line: u32::try_from(row).unwrap(),
            character: u32::try_from(text.find("diagrams").unwrap()).unwrap(),
        };

        assert_eq!(
            resolve_hover(&content, pos, &setup, Some(&ws)).as_deref(),
            Some("image::images/diagrams/arch.svg[Architecture]\n\n\u{2713} File exists")
        );
    }

    #[rstest]
    #[case::block(
        "= Doc A\n\nimage::logo.png[Logo,200]\n",
        10,
        "image::logo.png[Logo,200]"
    )]
    #[case::inline(
        "= Doc A\n:product: Acme\n\nText image:logo.png[{product} logo,32] here.\n",
        12,
        "image:logo.png[{product} logo,32]"
    )]
    #[case::empty("= Doc A\n\nimage::logo.png[]\n", 10, "image::logo.png[]")]
    fn test_hover_image_keeps_the_attribute_list_as_written(
        #[case] doc: &str,
        #[case] character: u32,
        #[case] first_line: &str,
    ) {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        fs::write(&a, doc).unwrap();
        fs::write(root.join("logo.png"), "fake-png-data").unwrap();
        let ws = Workspace::build(&root, &a);
        let macro_line = doc.lines().position(|l| l.contains("image:")).unwrap();
        let pos = Position {
            line: u32::try_from(macro_line).unwrap(),
            character,
        };

        let hover = resolve_hover(doc, pos, &a, Some(&ws)).unwrap();

        assert_eq!(hover, format!("{first_line}\n\n\u{2713} File exists"));
    }

    #[test]
    fn test_hover_image_outside_the_document_folder_stays_absolute() {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        let doc = "= Doc A\n:imagesdir: /asciidoc-lsp-absent/images\n\nimage::logo.png[Logo]\n";
        fs::write(&a, doc).unwrap();
        let ws = Workspace::build(&root, &a);
        let pos = Position {
            line: 3,
            character: 10,
        };

        let hover = resolve_hover(doc, pos, &a, Some(&ws)).unwrap();

        assert_eq!(
            hover,
            "image::/asciidoc-lsp-absent/images/logo.png[Logo]\n\n\u{2717} File not found"
        );
    }

    #[test]
    fn test_macro_attrlist_of_an_unfinished_macro_is_empty() {
        let (_tmp, root) = canonical_tmp();
        let a = root.join("a.adoc");
        let doc = "= Doc A\n\nimage::logo.png[Alt\n";
        fs::write(&a, doc).unwrap();
        let ws = Workspace::build(&root, &a);
        let pos = Position {
            line: 2,
            character: 10,
        };

        let hover = resolve_hover(doc, pos, &a, Some(&ws));

        assert!(
            hover
                .as_deref()
                .is_none_or(|h| h.starts_with("image::logo.png[]")),
            "got: {hover:?}"
        );
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
            hover.contains("image::photo.png[My Photo]"),
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
            hover.contains("image::missing.png[Alt]"),
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
            hover.contains("image::assets/img/logo.png[Logo]"),
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
        assert_eq!(hover, "image:img/logo.png[Alt]\n\n\u{2713} File exists");
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

    #[rstest]
    #[case::double_bracket_with_reftext("[[a,Ref]]", "a", Some((2, 3)))]
    #[case::shorthand("[#sid]", "sid", Some((2, 5)))]
    #[case::shorthand_with_role("[#sid.role]", "sid", Some((2, 5)))]
    #[case::shorthand_after_style("[source#cid,rust]", "cid", Some((8, 11)))]
    #[case::last_shorthand("[#a.r1#b]", "b", Some((7, 8)))]
    #[case::named_id("[id=sid]", "sid", Some((4, 7)))]
    #[case::quoted_named_id("[id=\"sid\"]", "sid", Some((5, 8)))]
    #[case::named_id_after_style("[source,rust,id=cid]", "cid", Some((16, 19)))]
    #[case::prefix_of_longer_id("[#sidx]", "sid", None)]
    #[case::inside_reftext("[#a,reftext=\"#b\"]", "b", None)]
    fn test_find_anchor_def_columns_attribute_lists(
        #[case] line: &str,
        #[case] id: &str,
        #[case] expected: Option<(usize, usize)>,
    ) {
        assert_eq!(find_anchor_def_columns(line, id), expected);
    }

    #[rstest]
    #[case::on_shorthand_id("[#sid]\n== S\n", 0, 3, Some("sid"))]
    #[case::on_named_id_value("[id=sid]\n== S\n", 0, 5, Some("sid"))]
    #[case::on_id_after_style("[source#cid,rust]\n----\nx\n----\n", 0, 9, Some("cid"))]
    #[case::on_style_is_no_anchor("[source#cid,rust]\n----\nx\n----\n", 0, 2, None)]
    #[case::on_other_value_is_no_anchor("[source#cid,rust]\n----\nx\n----\n", 0, 14, None)]
    fn test_find_node_at_position_on_attribute_list_id(
        #[case] content: &str,
        #[case] line: u32,
        #[case] col: u32,
        #[case] expected: Option<&str>,
    ) {
        let info = find_node_at_position(content, Position::new(line, col));
        let got = info.and_then(|i| match i.kind {
            NodeKind::Anchor { id } => Some(id),
            _ => None,
        });
        assert_eq!(got.as_deref(), expected);
    }

    #[rstest]
    #[case::empty_reftext("A anchor:aid[] b", "aid", Some((9, 12)))]
    #[case::with_reftext("A anchor:aid[Some Text] b", "aid", Some((9, 12)))]
    #[case::prefix_of_longer_id("anchor:aidx[]", "aid", None)]
    fn test_find_anchor_def_columns_anchor_macro(
        #[case] line: &str,
        #[case] id: &str,
        #[case] expected: Option<(usize, usize)>,
    ) {
        assert_eq!(find_anchor_def_columns(line, id), expected);
    }

    #[rstest]
    #[case::on_id("A anchor:aid[] b\n", 0, 10, Some("aid"))]
    #[case::on_id_with_reftext("A anchor:aid[Text] b\n", 0, 10, Some("aid"))]
    #[case::empty_target("A anchor:[] b\n", 0, 4, None)]
    #[case::invalid_target("A anchor:1d[] b\n", 0, 4, None)]
    fn test_find_node_at_position_on_anchor_macro(
        #[case] content: &str,
        #[case] line: u32,
        #[case] col: u32,
        #[case] expected: Option<&str>,
    ) {
        let info = find_node_at_position(content, Position::new(line, col));
        let got = info.and_then(|i| match i.kind {
            NodeKind::Anchor { id } => Some(id),
            _ => None,
        });
        assert_eq!(got.as_deref(), expected);
    }

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

    fn conflict_at(source: &str, line: u32, character: u32, new_name: &str) -> Option<String> {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        fs::write(&a, source).unwrap();
        let ws = Workspace::build(tmp.path(), &a);
        rename_conflict(
            source,
            Position { line, character },
            &a,
            Some(&ws),
            new_name,
        )
    }

    #[rstest::rstest]
    #[case::id_with_space("two words", Some("`two words` is not a valid ID"))]
    #[case::existing_id("other", Some("ID `other` is already defined"))]
    #[case::existing_section_id("_more", Some("ID `_more` is already defined"))]
    #[case::same_id("my-anchor", None)]
    #[case::free_id("fresh", None)]
    fn test_rename_conflict_anchor(#[case] new_name: &str, #[case] expected: Option<&str>) {
        let source = "= T\n\n[[my-anchor]]\n== Section\n\n[[other]]\nText.\n\n== More\n";
        assert_eq!(conflict_at(source, 2, 4, new_name).as_deref(), expected);
    }

    #[rstest::rstest]
    #[case::name_with_space("two words", Some("`two words` is not a valid attribute name"))]
    #[case::builtin("toc", Some("`toc` is a built-in attribute"))]
    #[case::existing("version", Some("attribute `version` is already defined"))]
    #[case::same("product", None)]
    #[case::free("brand", None)]
    fn test_rename_conflict_attribute(#[case] new_name: &str, #[case] expected: Option<&str>) {
        let source = "= T\n:product: X\n:version: 1\n\n{product} {version}\n";
        assert_eq!(conflict_at(source, 4, 2, new_name).as_deref(), expected);
    }

    #[test]
    fn test_prepare_rename_rejects_builtin_attribute() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let source = "= T\n:toc: left\n";
        fs::write(&a, source).unwrap();
        let ws = Workspace::build(tmp.path(), &a);

        let pos = Position {
            line: 1,
            character: 2,
        };
        assert_eq!(prepare_rename(source, pos, &a, Some(&ws)), None);
    }

    /// `<<Command line>>` names the anchor `cli` by its reftext; renaming
    /// `cli` would turn the text into an ID reference.
    #[test]
    fn test_prepare_rename_rejects_natural_xref() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/natural_xrefs")
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        let path = root.join("main.adoc");
        let content = fs::read_to_string(&path).unwrap();

        let pos = Position {
            line: 19,
            character: 42,
        };
        assert_eq!(prepare_rename(&content, pos, &path, Some(&ws)), None);
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

    const ANCHOR_MACRO_FIXTURE: &str = "../../tests/fixtures/inline_anchor_macro";

    #[test]
    fn test_resolve_rename_from_xref_edits_anchor_macro_definition() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(ANCHOR_MACRO_FIXTURE)
            .canonicalize()
            .unwrap();
        let index = root.join("index.adoc");
        let ws = Workspace::build(&root, &index);
        let content = fs::read_to_string(&index).unwrap();
        let line = content
            .lines()
            .position(|l| l.starts_with("See <<plain>>"))
            .unwrap() as u32;
        let character = content
            .lines()
            .nth(line as usize)
            .unwrap()
            .find("withtext")
            .unwrap() as u32
            + 2;

        let edits = resolve_rename(
            &content,
            Position { line, character },
            &index,
            Some(&ws),
            &HashMap::new(),
        )
        .unwrap();
        let file_edits = edits.get(&index).unwrap();
        assert_eq!(file_edits.len(), 2);
        for (l, start, end) in file_edits {
            let text = content.lines().nth(*l - 1).unwrap();
            assert_eq!(&text[*start..*end], "withtext", "line {l}");
        }
        assert!(file_edits.iter().any(|(l, _, _)| *l == 5));
    }

    #[test]
    fn test_goto_definition_from_xref_lands_on_anchor_macro() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(ANCHOR_MACRO_FIXTURE)
            .canonicalize()
            .unwrap();
        let index = root.join("index.adoc");
        let ws = Workspace::build(&root, &index);
        let content = fs::read_to_string(&index).unwrap();
        let line = content
            .lines()
            .position(|l| l.starts_with("See <<plain>>"))
            .unwrap() as u32;

        let defs =
            resolve_goto_definitions(&content, Position { line, character: 8 }, &index, Some(&ws));
        assert_eq!(defs, [(index, 5)]);
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

        // Without overrides the xref still has its span from the workspace, but
        // the stale disk line holds no `[[new-id]]`, so the definition is missed.
        let result_no_override =
            resolve_rename(new_content, pos, &a_canon, Some(&ws), &HashMap::new()).unwrap();
        assert_eq!(
            result_no_override.get(&a_canon).map(Vec::len),
            Some(1),
            "without overrides only the xref is found, got: {result_no_override:?}"
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
        let def = &report["resolution"]["scopes"][0]["definition"];
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
            node_kind_name(&NodeKind::AttributeDef {
                name: s(),
                unset: false
            }),
            "attribute_def"
        );
        assert_eq!(node_kind_name(&NodeKind::Anchor { id: s() }), "anchor");
        assert_eq!(
            node_kind_name(&NodeKind::Image {
                path: s(),
                inline: false,
                attrlist: s()
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
