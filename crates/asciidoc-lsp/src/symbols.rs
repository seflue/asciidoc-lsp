use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tower_lsp_server::ls_types::{
    DocumentSymbol, Location, Range, SymbolInformation, SymbolKind, Uri,
};

use asciidoc_core::workspace::Workspace;

use crate::positions;

// ---------------------------------------------------------------------------
// DocumentSymbol helpers (V-8: extracted for testability)
// ---------------------------------------------------------------------------

/// Convert a flat list of sections into a nested `DocumentSymbol` hierarchy.
///
/// Sections are nested by level: a level-2 section becomes a child of the
/// preceding level-1 section, etc. Range covers from heading to just before
/// the next section at the same or lower level.
pub(crate) fn sections_to_document_symbols(
    sections: &[asciidoc_core::types::Section],
    content: &str,
) -> Vec<DocumentSymbol> {
    let total_lines = content.lines().count();
    let mut idx = 0;
    build_symbol_children(sections, &mut idx, None, content, total_lines)
}

fn build_symbol_children(
    sections: &[asciidoc_core::types::Section],
    idx: &mut usize,
    parent_level: Option<usize>,
    content: &str,
    total_lines: usize,
) -> Vec<DocumentSymbol> {
    let mut result = Vec::new();

    while *idx < sections.len() {
        let section = &sections[*idx];

        // If this section is at or above the parent level, it belongs to an ancestor
        if let Some(parent) = parent_level {
            if section.level <= parent {
                break;
            }
        }

        let current_level = section.level;
        let start_line = section.line; // 1-indexed
        let title = section.title.clone();
        *idx += 1;

        // Recursively collect children (deeper levels)
        let children =
            build_symbol_children(sections, idx, Some(current_level), content, total_lines);

        // Range end: line before the next section at same/higher level, or end of document
        let end_line = if *idx < sections.len() {
            sections[*idx].line.saturating_sub(1).max(start_line)
        } else {
            total_lines
        };

        let start_pos = positions::to_lsp_position(start_line, 0, content);
        let end_pos = positions::to_lsp_position(end_line, 0, content);

        // selection_range: just the heading line
        let heading_len = content
            .lines()
            .nth(start_line.saturating_sub(1))
            .map_or(0, str::len);
        let selection_end = positions::to_lsp_position(start_line, heading_len, content);

        let range = Range {
            start: start_pos,
            end: end_pos,
        };
        let selection_range = Range {
            start: start_pos,
            end: selection_end,
        };

        #[allow(deprecated)]
        let symbol = DocumentSymbol {
            name: title,
            detail: None,
            kind: SymbolKind::STRUCT,
            tags: None,
            deprecated: None,
            range,
            selection_range,
            children: if children.is_empty() {
                None
            } else {
                Some(children)
            },
        };

        result.push(symbol);
    }

    result
}

/// Build `DocumentSymbol` list combining sections (nested), includes (flat), and attributes (flat).
pub(crate) fn build_document_symbols(
    sections: &[asciidoc_core::types::Section],
    includes: &[asciidoc_core::types::IncludeMacro],
    attributes: &[asciidoc_core::types::AttributeDef],
    content: &str,
) -> Vec<DocumentSymbol> {
    let mut symbols = sections_to_document_symbols(sections, content);
    symbols.extend(includes_to_document_symbols(includes, content));
    symbols.extend(attributes_to_document_symbols(attributes, content));
    symbols
}

/// Build a single-line `DocumentSymbol` whose range spans one source line.
///
/// Computes `start_pos` and `end_pos` (via line length) internally so callers
/// only need to supply the semantic fields.
fn line_span_document_symbol(
    name: String,
    detail: Option<String>,
    kind: SymbolKind,
    line: usize,
    content: &str,
) -> DocumentSymbol {
    let start_pos = positions::to_lsp_position(line, 0, content);
    let line_text = content
        .lines()
        .nth(line.saturating_sub(1))
        .unwrap_or_default();
    let end_pos = positions::to_lsp_position(line, line_text.len(), content);
    let range = Range {
        start: start_pos,
        end: end_pos,
    };

    #[allow(deprecated)]
    DocumentSymbol {
        name,
        detail,
        kind,
        tags: None,
        deprecated: None,
        range,
        selection_range: range,
        children: None,
    }
}

/// Convert includes to flat `DocumentSymbol` entries (`SymbolKind::FILE`).
pub(crate) fn includes_to_document_symbols(
    includes: &[asciidoc_core::types::IncludeMacro],
    content: &str,
) -> Vec<DocumentSymbol> {
    includes
        .iter()
        .map(|inc| {
            let detail = if inc.attributes.is_empty() {
                None
            } else {
                Some(
                    inc.attributes
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(","),
                )
            };
            line_span_document_symbol(
                inc.path.clone(),
                detail,
                SymbolKind::FILE,
                inc.line,
                content,
            )
        })
        .collect()
}

/// Convert attribute definitions to flat `DocumentSymbol` entries (`SymbolKind::VARIABLE`).
pub(crate) fn attributes_to_document_symbols(
    attributes: &[asciidoc_core::types::AttributeDef],
    content: &str,
) -> Vec<DocumentSymbol> {
    attributes
        .iter()
        .map(|attr| {
            let detail = if attr.value.is_empty() {
                None
            } else {
                Some(attr.value.clone())
            };
            line_span_document_symbol(
                attr.name.clone(),
                detail,
                SymbolKind::VARIABLE,
                attr.line,
                content,
            )
        })
        .collect()
}

/// Convert a filesystem path to an LSP URI.
pub(crate) fn path_to_uri(path: &Path) -> Option<Uri> {
    Uri::from_file_path(path)
}

// ---------------------------------------------------------------------------
// Workspace Symbols (V-8: extracted for testability)
// ---------------------------------------------------------------------------

/// Build a `SymbolInformation` for a single workspace symbol.
///
/// Centralises the `#[allow(deprecated)]` workaround and the
/// `positions::to_lsp_position` → `Location` → `SymbolInformation` chain
/// that was previously duplicated in every loop of `collect_workspace_symbols`.
fn make_symbol_info(
    name: String,
    kind: SymbolKind,
    uri: &Uri,
    line: usize,
    content: &str,
) -> SymbolInformation {
    let pos = positions::to_lsp_position(line, 0, content);
    #[allow(deprecated)]
    SymbolInformation {
        name,
        kind,
        tags: None,
        deprecated: None,
        location: Location {
            uri: uri.clone(),
            range: Range {
                start: pos,
                end: pos,
            },
        },
        container_name: None,
    }
}

/// Collect workspace symbols (sections + anchors + includes + attributes) matching a query.
///
/// - Sections become `SymbolKind::STRUCT` symbols
/// - Anchors become `SymbolKind::CONSTANT` symbols
/// - Includes become `SymbolKind::FILE` symbols
/// - Attributes become `SymbolKind::VARIABLE` symbols
/// - Empty query returns all symbols
/// - Non-empty query filters case-insensitively (name contains query)
///
/// Each symbol gets a `Location` built from the file URI and 1-indexed line
/// converted to an LSP Position via `positions::to_lsp_position`.
pub(crate) fn collect_workspace_symbols(ws: &Workspace, query: &str) -> Vec<SymbolInformation> {
    let query_lower = query.to_lowercase();
    let mut symbols = Vec::new();
    let mut file_cache: HashMap<PathBuf, String> = HashMap::new();

    for (path, doc) in ws.documents() {
        let Some(uri) = path_to_uri(path) else {
            continue;
        };
        let content = file_cache
            .entry(path.clone())
            .or_insert_with(|| std::fs::read_to_string(path).unwrap_or_default());

        // Sections
        for section in &doc.sections {
            if !query_lower.is_empty() && !section.title.to_lowercase().contains(&query_lower) {
                continue;
            }
            symbols.push(make_symbol_info(
                section.title.clone(),
                SymbolKind::STRUCT,
                &uri,
                section.line,
                content,
            ));
        }

        // Includes
        for inc in &doc.includes {
            if !query_lower.is_empty() && !inc.path.to_lowercase().contains(&query_lower) {
                continue;
            }
            symbols.push(make_symbol_info(
                inc.path.clone(),
                SymbolKind::FILE,
                &uri,
                inc.line,
                content,
            ));
        }

        // Attributes
        for attr in &doc.attributes {
            if !query_lower.is_empty() && !attr.name.to_lowercase().contains(&query_lower) {
                continue;
            }
            symbols.push(make_symbol_info(
                attr.name.clone(),
                SymbolKind::VARIABLE,
                &uri,
                attr.line,
                content,
            ));
        }
    }

    // Every definition of an ID, in each top-level document
    for (id, loc) in &ws.anchor_definitions() {
        if !query_lower.is_empty() && !id.to_lowercase().contains(&query_lower) {
            continue;
        }

        let Some(uri) = path_to_uri(&loc.file) else {
            continue;
        };
        let content = file_cache
            .entry(loc.file.clone())
            .or_insert_with(|| std::fs::read_to_string(&loc.file).unwrap_or_default());
        symbols.push(make_symbol_info(
            id.clone(),
            SymbolKind::CONSTANT,
            &uri,
            loc.line,
            content,
        ));
    }

    // Sort by name for deterministic output
    symbols.sort_by(|a, b| a.name.cmp(&b.name));
    symbols
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    use tower_lsp_server::ls_types::{DocumentSymbol, SymbolKind};

    // --- DocumentSymbol hierarchy tests (V-8) ---

    #[test]
    fn test_sections_to_document_symbols_flat() {
        let sections = vec![
            asciidoc_core::types::Section {
                level: 0,
                title: "Title".into(),
                line: 1,
                id: None,
                is_header: false,
                style: None,
                placement: asciidoc_core::types::Placement::Body,
            },
            asciidoc_core::types::Section {
                level: 1,
                title: "Chapter A".into(),
                line: 3,
                id: None,
                is_header: false,
                style: None,
                placement: asciidoc_core::types::Placement::Body,
            },
            asciidoc_core::types::Section {
                level: 1,
                title: "Chapter B".into(),
                line: 5,
                id: None,
                is_header: false,
                style: None,
                placement: asciidoc_core::types::Placement::Body,
            },
        ];
        let content = "= Title\n\n== Chapter A\n\n== Chapter B\n";

        let symbols = sections_to_document_symbols(&sections, content);

        // Top level: one root symbol (Title)
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "Title");
        assert_eq!(symbols[0].kind, SymbolKind::STRUCT);

        // Title has two children
        let children = symbols[0].children.as_ref().unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].name, "Chapter A");
        assert_eq!(children[1].name, "Chapter B");

        // Chapters have no children
        assert!(children[0].children.is_none());
        assert!(children[1].children.is_none());
    }

    #[test]
    fn test_sections_to_document_symbols_nested_3_levels() {
        let sections = vec![
            asciidoc_core::types::Section {
                level: 0,
                title: "Doc".into(),
                line: 1,
                id: None,
                is_header: false,
                style: None,
                placement: asciidoc_core::types::Placement::Body,
            },
            asciidoc_core::types::Section {
                level: 1,
                title: "Ch1".into(),
                line: 3,
                id: None,
                is_header: false,
                style: None,
                placement: asciidoc_core::types::Placement::Body,
            },
            asciidoc_core::types::Section {
                level: 2,
                title: "Sub1".into(),
                line: 5,
                id: None,
                is_header: false,
                style: None,
                placement: asciidoc_core::types::Placement::Body,
            },
            asciidoc_core::types::Section {
                level: 2,
                title: "Sub2".into(),
                line: 7,
                id: None,
                is_header: false,
                style: None,
                placement: asciidoc_core::types::Placement::Body,
            },
            asciidoc_core::types::Section {
                level: 1,
                title: "Ch2".into(),
                line: 9,
                id: None,
                is_header: false,
                style: None,
                placement: asciidoc_core::types::Placement::Body,
            },
        ];
        let content = "= Doc\n\n== Ch1\n\n=== Sub1\n\n=== Sub2\n\n== Ch2\n";

        let symbols = sections_to_document_symbols(&sections, content);

        assert_eq!(symbols.len(), 1);
        let root = &symbols[0];
        assert_eq!(root.name, "Doc");

        let chapters = root.children.as_ref().unwrap();
        assert_eq!(chapters.len(), 2);
        assert_eq!(chapters[0].name, "Ch1");
        assert_eq!(chapters[1].name, "Ch2");

        // Ch1 has two subsections
        let subs = chapters[0].children.as_ref().unwrap();
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].name, "Sub1");
        assert_eq!(subs[1].name, "Sub2");

        // Ch2 has no children
        assert!(chapters[1].children.is_none());
    }

    #[test]
    fn test_sections_to_document_symbols_positions() {
        let sections = vec![
            asciidoc_core::types::Section {
                level: 0,
                title: "Title".into(),
                line: 1,
                id: None,
                is_header: false,
                style: None,
                placement: asciidoc_core::types::Placement::Body,
            },
            asciidoc_core::types::Section {
                level: 1,
                title: "Chapter".into(),
                line: 3,
                id: None,
                is_header: false,
                style: None,
                placement: asciidoc_core::types::Placement::Body,
            },
        ];
        let content = "= Title\n\n== Chapter\n\nSome text.\n";

        let symbols = sections_to_document_symbols(&sections, content);
        let root = &symbols[0];

        // Root starts at line 0 (0-indexed), covers to end of document
        assert_eq!(root.range.start.line, 0);
        assert_eq!(root.range.start.character, 0);

        // selection_range covers the heading line
        assert_eq!(root.selection_range.start.line, 0);
        assert_eq!(root.selection_range.end.line, 0);
        assert_eq!(root.selection_range.end.character, 7); // "= Title" = 7 chars

        // Chapter starts at line 2 (0-indexed)
        let chapter = &root.children.as_ref().unwrap()[0];
        assert_eq!(chapter.range.start.line, 2);
        assert_eq!(chapter.selection_range.end.character, 10); // "== Chapter" = 10 chars
    }

    #[test]
    fn test_sections_to_document_symbols_empty() {
        let sections: Vec<asciidoc_core::types::Section> = vec![];
        let content = "Just plain text, no sections.\n";

        let symbols = sections_to_document_symbols(&sections, content);
        assert!(symbols.is_empty());
    }

    #[test]
    fn test_sections_to_document_symbols_no_root() {
        // Document starting at level 1 (no document title)
        let sections = vec![
            asciidoc_core::types::Section {
                level: 1,
                title: "Ch1".into(),
                line: 1,
                id: None,
                is_header: false,
                style: None,
                placement: asciidoc_core::types::Placement::Body,
            },
            asciidoc_core::types::Section {
                level: 1,
                title: "Ch2".into(),
                line: 3,
                id: None,
                is_header: false,
                style: None,
                placement: asciidoc_core::types::Placement::Body,
            },
        ];
        let content = "== Ch1\n\n== Ch2\n";

        let symbols = sections_to_document_symbols(&sections, content);

        // Both chapters are top-level
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "Ch1");
        assert_eq!(symbols[1].name, "Ch2");
    }

    #[test]
    fn test_sections_to_document_symbols_from_parsed_content() {
        // Full integration: parse real AsciiDoc → sections → DocumentSymbol
        let content = "= My Document\n\n== Introduction\n\nSome intro text.\n\n== Details\n\n=== Part A\n\n=== Part B\n";

        let mut parser = asciidoc_core::parser::init_block_parser().unwrap();
        let tree = asciidoc_core::parser::parse(&mut parser, content).unwrap();
        let sections =
            asciidoc_core::extractors::extract_sections(tree.root_node(), content.as_bytes());

        let symbols = sections_to_document_symbols(&sections, content);

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "My Document");

        let chapters = symbols[0].children.as_ref().unwrap();
        assert_eq!(chapters.len(), 2);
        assert_eq!(chapters[0].name, "Introduction");
        assert_eq!(chapters[1].name, "Details");

        let parts = chapters[1].children.as_ref().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].name, "Part A");
        assert_eq!(parts[1].name, "Part B");
    }

    // --- Workspace Symbols tests (V-8) ---

    /// Helper: build a workspace from temp files for workspace symbol tests.
    fn build_test_workspace(files: &[(&str, &str)]) -> (tempfile::TempDir, Workspace) {
        let tmp = tempfile::tempdir().unwrap();
        let mut first_file = None;
        for (name, content) in files {
            let path = tmp.path().join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, content).unwrap();
            if first_file.is_none() {
                first_file = Some(path);
            }
        }
        let entry = first_file.unwrap();
        let ws = Workspace::build(tmp.path(), &entry);
        (tmp, ws)
    }

    #[test]
    fn test_workspace_symbols_empty_query_returns_all() {
        let (_tmp, ws) =
            build_test_workspace(&[("index.adoc", "= Title\n\n== Chapter A\n\n== Chapter B\n")]);

        let symbols = collect_workspace_symbols(&ws, "");

        // Should contain sections and their implicit anchor IDs
        assert!(!symbols.is_empty());
        // All sections should be present
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Title"), "missing 'Title' in {:?}", names);
        assert!(
            names.contains(&"Chapter A"),
            "missing 'Chapter A' in {:?}",
            names
        );
        assert!(
            names.contains(&"Chapter B"),
            "missing 'Chapter B' in {:?}",
            names
        );
    }

    #[test]
    fn test_workspace_symbols_query_filters_case_insensitive() {
        let (_tmp, ws) =
            build_test_workspace(&[("index.adoc", "= Title\n\n== Introduction\n\n== Details\n")]);

        let symbols = collect_workspace_symbols(&ws, "intro");

        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"Introduction"),
            "missing 'Introduction' in {:?}",
            names
        );
        // "Title" and "Details" should NOT match "intro"
        assert!(
            !names.contains(&"Title"),
            "unexpected 'Title' in {:?}",
            names
        );
        assert!(
            !names.contains(&"Details"),
            "unexpected 'Details' in {:?}",
            names
        );
    }

    #[test]
    fn test_workspace_symbols_query_no_match() {
        let (_tmp, ws) = build_test_workspace(&[("index.adoc", "= Title\n\n== Chapter\n")]);

        let symbols = collect_workspace_symbols(&ws, "zzzznotfound");
        assert!(symbols.is_empty());
    }

    #[test]
    fn test_workspace_symbols_sections_use_string_kind() {
        let (_tmp, ws) = build_test_workspace(&[("index.adoc", "= Title\n\n== Chapter\n")]);

        let symbols = collect_workspace_symbols(&ws, "");
        let section_symbols: Vec<_> = symbols
            .iter()
            .filter(|s| s.name == "Title" || s.name == "Chapter")
            .collect();

        for sym in &section_symbols {
            assert_eq!(
                sym.kind,
                SymbolKind::STRUCT,
                "section '{}' should be STRING",
                sym.name
            );
        }
    }

    #[test]
    fn test_workspace_symbols_anchors_use_key_kind() {
        let (_tmp, ws) =
            build_test_workspace(&[("index.adoc", "= Title\n\n[[my-anchor]]\n== Chapter\n")]);

        let symbols = collect_workspace_symbols(&ws, "my-anchor");
        let anchor_sym = symbols.iter().find(|s| s.name == "my-anchor");
        assert!(anchor_sym.is_some(), "expected anchor symbol 'my-anchor'");
        assert_eq!(anchor_sym.unwrap().kind, SymbolKind::CONSTANT);
    }

    /// `shared` is defined in `setup.adoc`, `reference.adoc` (both read by
    /// `book.adoc`, where the second is a duplicate) and `standalone.adoc`:
    /// the symbol list has every definition, not one per ID.
    #[test]
    fn test_workspace_symbols_list_every_definition_of_an_id() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/anchor_scope")
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();

        let mut found: Vec<(String, u32)> = collect_workspace_symbols(&ws, "shared")
            .into_iter()
            .filter(|symbol| symbol.name == "shared")
            .map(|symbol| {
                let file = symbol.location.uri.path().as_str().to_string();
                (
                    file.strip_prefix(root.to_str().unwrap())
                        .unwrap()
                        .to_string(),
                    symbol.location.range.start.line,
                )
            })
            .collect();
        found.sort();

        assert_eq!(
            found,
            vec![
                ("/chapters/reference.adoc".to_string(), 0),
                ("/chapters/setup.adoc".to_string(), 0),
                ("/standalone.adoc".to_string(), 2),
            ]
        );
    }

    #[test]
    fn test_workspace_symbols_multi_file() {
        let (_tmp, ws) = build_test_workspace(&[
            ("index.adoc", "= Root Doc\n\ninclude::sub/chapter.adoc[]\n"),
            ("sub/chapter.adoc", "== Sub Chapter\n\nContent.\n"),
        ]);

        let symbols = collect_workspace_symbols(&ws, "");
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"Root Doc"),
            "missing 'Root Doc' in {:?}",
            names
        );
        assert!(
            names.contains(&"Sub Chapter"),
            "missing 'Sub Chapter' in {:?}",
            names
        );
    }

    #[test]
    fn test_workspace_symbols_sorted_by_name() {
        let (_tmp, ws) =
            build_test_workspace(&[("index.adoc", "= Zebra Doc\n\n== Alpha\n\n== Beta\n")]);

        let symbols = collect_workspace_symbols(&ws, "");
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();

        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "symbols should be sorted by name");
    }

    #[test]
    fn test_workspace_symbols_location_has_correct_uri() {
        let (_tmp, ws) = build_test_workspace(&[("index.adoc", "= Title\n")]);

        let symbols = collect_workspace_symbols(&ws, "Title");
        assert!(!symbols.is_empty());
        let uri_str = symbols[0].location.uri.to_string();
        assert!(
            uri_str.starts_with("file://"),
            "URI should be file://: {}",
            uri_str
        );
        assert!(
            uri_str.contains("index.adoc"),
            "URI should contain filename: {}",
            uri_str
        );
    }

    // --- workspace/symbol — Include Symbols ---

    #[test]
    fn test_workspace_symbols_includes_as_file_kind() {
        let (_tmp, ws) = build_test_workspace(&[
            ("index.adoc", "= Root\n\ninclude::chapters/intro.adoc[]\n"),
            ("chapters/intro.adoc", "== Intro\n\nContent.\n"),
        ]);

        let symbols = collect_workspace_symbols(&ws, "");
        let file_symbols: Vec<_> = symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::FILE)
            .collect();
        assert!(
            !file_symbols.is_empty(),
            "expected at least one FILE symbol for include"
        );
        assert!(
            file_symbols.iter().any(|s| s.name.contains("intro.adoc")),
            "expected include path containing 'intro.adoc', got: {:?}",
            file_symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_workspace_symbols_include_query_filter() {
        let (_tmp, ws) = build_test_workspace(&[
            ("index.adoc", "= Root\n\ninclude::chapters/intro.adoc[]\n"),
            ("chapters/intro.adoc", "== Intro\n\nContent.\n"),
        ]);

        let symbols = collect_workspace_symbols(&ws, "intro");
        let file_symbols: Vec<_> = symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::FILE)
            .collect();
        assert!(
            !file_symbols.is_empty(),
            "query 'intro' should match include path"
        );
    }

    // --- workspace/symbol — Attribute Symbols ---

    #[test]
    fn test_workspace_symbols_attributes_as_variable_kind() {
        let (_tmp, ws) =
            build_test_workspace(&[("index.adoc", "= Title\n:my-attr: some-value\n\n== Sec\n")]);

        let symbols = collect_workspace_symbols(&ws, "");
        let var_symbols: Vec<_> = symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::VARIABLE)
            .collect();
        assert!(
            !var_symbols.is_empty(),
            "expected at least one VARIABLE symbol for attribute"
        );
        assert!(
            var_symbols.iter().any(|s| s.name == "my-attr"),
            "expected attribute 'my-attr', got: {:?}",
            var_symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_workspace_symbols_attribute_query_filter() {
        let (_tmp, ws) =
            build_test_workspace(&[("index.adoc", "= Title\n:my-attr: value\n\n== Sec\n")]);

        let symbols = collect_workspace_symbols(&ws, "my-attr");
        let var_symbols: Vec<_> = symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::VARIABLE)
            .collect();
        assert!(
            !var_symbols.is_empty(),
            "query 'my-attr' should match attribute name"
        );
    }

    // -----------------------------------------------------------------------
    // Integration Tests — arc42 Fixture
    // -----------------------------------------------------------------------

    /// Path to the workspace root (asciidoc-lsp/) from the crate manifest dir.
    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("workspace root should exist")
    }

    /// Path to the arc42 fixture docs directory.
    fn arc42_docs_dir() -> PathBuf {
        workspace_root().join("docs")
    }

    #[test]
    fn test_integration_arc42_document_symbol() {
        let docs_dir = arc42_docs_dir();
        let index = docs_dir.join("index.adoc");
        assert!(index.exists(), "arc42 index.adoc should exist");

        let content = fs::read_to_string(&index).unwrap();

        // Parse and extract sections from the entry file
        let mut parser = asciidoc_core::parser::init_block_parser().unwrap();
        let tree = asciidoc_core::parser::parse(&mut parser, &content).unwrap();
        let sections =
            asciidoc_core::extractors::extract_sections(tree.root_node(), content.as_bytes());

        // The index.adoc should have the document title as its only section
        assert!(
            !sections.is_empty(),
            "arc42 index.adoc should have sections"
        );
        assert_eq!(
            sections[0].title, "asciidoc-lsp Architecture Documentation",
            "document title should match"
        );

        // Convert to DocumentSymbols — the hierarchy should work
        let symbols = sections_to_document_symbols(&sections, &content);
        assert!(!symbols.is_empty(), "should produce document symbols");
        assert_eq!(symbols[0].name, "asciidoc-lsp Architecture Documentation");
        assert_eq!(symbols[0].kind, SymbolKind::STRUCT);

        // Verify an included chapter file also produces valid symbols
        let chapter = docs_dir.join("chapters/01_introduction_and_goals.adoc");
        let ch_content = fs::read_to_string(&chapter).unwrap();
        let ch_tree = asciidoc_core::parser::parse(&mut parser, &ch_content).unwrap();
        let ch_sections =
            asciidoc_core::extractors::extract_sections(ch_tree.root_node(), ch_content.as_bytes());

        assert!(
            ch_sections.len() >= 3,
            "introduction chapter should have multiple sections, found: {}",
            ch_sections.len()
        );

        let ch_symbols = sections_to_document_symbols(&ch_sections, &ch_content);
        assert!(
            !ch_symbols.is_empty(),
            "chapter should produce document symbols"
        );
    }

    #[test]
    fn test_integration_arc42_workspace_symbols() {
        let ws_root = workspace_root();
        let entry_path = ws_root.join("docs/index.adoc");

        let ws = Workspace::build(&ws_root, &entry_path);

        // All symbols (empty query)
        let all_symbols = collect_workspace_symbols(&ws, "");
        assert!(
            all_symbols.len() >= 10,
            "arc42 workspace should have at least 10 symbols (sections + anchors), found: {}",
            all_symbols.len()
        );

        // Filtered query: "goal" should match "Project Goals" section
        let goal_symbols = collect_workspace_symbols(&ws, "goal");
        assert!(
            !goal_symbols.is_empty(),
            "query 'goal' should match at least one symbol"
        );
        let goal_names: Vec<&str> = goal_symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            goal_names.iter().any(|n| n.contains("Goal")),
            "should find 'Goal'-containing symbol, got: {:?}",
            goal_names
        );

        // All symbols should have valid URIs
        for sym in &all_symbols {
            let uri_str = sym.location.uri.to_string();
            assert!(
                uri_str.starts_with("file://"),
                "symbol URI should be file://: {}",
                uri_str
            );
        }

        // Includes should appear as FILE symbols
        let file_symbols: Vec<_> = all_symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::FILE)
            .collect();
        assert!(
            !file_symbols.is_empty(),
            "arc42 workspace should have FILE symbols for includes"
        );

        // Attributes should appear as VARIABLE symbols
        let var_symbols: Vec<_> = all_symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::VARIABLE)
            .collect();
        assert!(
            !var_symbols.is_empty(),
            "arc42 workspace should have VARIABLE symbols for attributes"
        );
    }

    // --- path_to_uri tests (Finding 2: URL-encoding) ---

    #[test]
    fn test_path_to_uri_simple_path() {
        let uri = path_to_uri(std::path::Path::new("/home/user/file.adoc")).unwrap();
        assert_eq!(uri.to_string(), "file:///home/user/file.adoc");
    }

    #[test]
    fn test_path_to_uri_encodes_spaces() {
        let uri = path_to_uri(std::path::Path::new("/home/user/my project/file.adoc")).unwrap();
        let uri_str = uri.to_string();
        assert!(
            !uri_str.contains(' '),
            "URI must not contain raw spaces: {uri_str}"
        );
        assert!(
            uri_str.contains("my%20project"),
            "spaces should be percent-encoded: {uri_str}"
        );
    }

    #[test]
    fn test_path_to_uri_encodes_umlauts() {
        let uri = path_to_uri(std::path::Path::new("/home/müller/file.adoc")).unwrap();
        let uri_str = uri.to_string();
        assert!(
            !uri_str.contains('ü'),
            "URI must not contain raw umlauts: {uri_str}"
        );
        assert!(
            uri_str.contains("m%C3%BCller"),
            "umlauts should be percent-encoded: {uri_str}"
        );
    }

    // --- documentSymbol — Include Symbols ---

    #[test]
    fn test_document_symbols_include_as_file_kind() {
        let content = "= Title\n\ninclude::sub/chapter.adoc[]\n\n== Section\n";

        let mut parser = asciidoc_core::parser::init_block_parser().unwrap();
        let tree = asciidoc_core::parser::parse(&mut parser, content).unwrap();
        let sections =
            asciidoc_core::extractors::extract_sections(tree.root_node(), content.as_bytes());
        let includes =
            asciidoc_core::extractors::extract_includes(tree.root_node(), content.as_bytes());
        let attributes =
            asciidoc_core::extractors::extract_attributes(tree.root_node(), content.as_bytes());

        let symbols = build_document_symbols(&sections, &includes, &attributes, content);

        // Should contain at least one FILE symbol for the include
        let file_symbols: Vec<_> = symbols_flat(&symbols)
            .into_iter()
            .filter(|s| s.kind == SymbolKind::FILE)
            .collect();
        assert_eq!(
            file_symbols.len(),
            1,
            "expected 1 FILE symbol for include, got: {:?}",
            file_symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert_eq!(file_symbols[0].name, "sub/chapter.adoc");
    }

    #[test]
    fn test_document_symbols_include_position() {
        let content = "= Title\n\ninclude::sub/chapter.adoc[]\n\n== Section\n";

        let mut parser = asciidoc_core::parser::init_block_parser().unwrap();
        let tree = asciidoc_core::parser::parse(&mut parser, content).unwrap();
        let sections =
            asciidoc_core::extractors::extract_sections(tree.root_node(), content.as_bytes());
        let includes =
            asciidoc_core::extractors::extract_includes(tree.root_node(), content.as_bytes());
        let attributes =
            asciidoc_core::extractors::extract_attributes(tree.root_node(), content.as_bytes());

        let symbols = build_document_symbols(&sections, &includes, &attributes, content);

        let file_sym = symbols_flat(&symbols)
            .into_iter()
            .find(|s| s.kind == SymbolKind::FILE)
            .expect("should have a FILE symbol");
        // include::sub/chapter.adoc[] is on line 3 (1-indexed) → LSP line 2 (0-indexed)
        assert_eq!(file_sym.range.start.line, 2);
    }

    // --- documentSymbol — Attribute Symbols ---

    #[test]
    fn test_document_symbols_attribute_as_variable_kind() {
        let content = "= Title\n:source-dir: ../src\n\n== Section\n";

        let mut parser = asciidoc_core::parser::init_block_parser().unwrap();
        let tree = asciidoc_core::parser::parse(&mut parser, content).unwrap();
        let sections =
            asciidoc_core::extractors::extract_sections(tree.root_node(), content.as_bytes());
        let includes =
            asciidoc_core::extractors::extract_includes(tree.root_node(), content.as_bytes());
        let attributes =
            asciidoc_core::extractors::extract_attributes(tree.root_node(), content.as_bytes());

        let symbols = build_document_symbols(&sections, &includes, &attributes, content);

        let var_symbols: Vec<_> = symbols_flat(&symbols)
            .into_iter()
            .filter(|s| s.kind == SymbolKind::VARIABLE)
            .collect();
        assert_eq!(
            var_symbols.len(),
            1,
            "expected 1 VARIABLE symbol for attribute"
        );
        assert_eq!(var_symbols[0].name, "source-dir");
        assert_eq!(var_symbols[0].detail.as_deref(), Some("../src"));
    }

    #[test]
    fn test_document_symbols_mixed_sections_includes_attributes() {
        let content =
            "= Title\n:imagesdir: images\n:source-dir: ../src\n\ninclude::ch1.adoc[]\n\n== Section\n\ninclude::ch2.adoc[leveloffset=+1]\n";

        let mut parser = asciidoc_core::parser::init_block_parser().unwrap();
        let tree = asciidoc_core::parser::parse(&mut parser, content).unwrap();
        let sections =
            asciidoc_core::extractors::extract_sections(tree.root_node(), content.as_bytes());
        let includes =
            asciidoc_core::extractors::extract_includes(tree.root_node(), content.as_bytes());
        let attributes =
            asciidoc_core::extractors::extract_attributes(tree.root_node(), content.as_bytes());

        let symbols = build_document_symbols(&sections, &includes, &attributes, content);
        let flat = symbols_flat(&symbols);

        // Count by kind
        let struct_count = flat.iter().filter(|s| s.kind == SymbolKind::STRUCT).count();
        let file_count = flat.iter().filter(|s| s.kind == SymbolKind::FILE).count();
        let var_count = flat
            .iter()
            .filter(|s| s.kind == SymbolKind::VARIABLE)
            .count();

        assert_eq!(
            struct_count, 2,
            "expected 2 STRUCT symbols (Title + Section)"
        );
        assert_eq!(file_count, 2, "expected 2 FILE symbols (ch1 + ch2)");
        assert_eq!(
            var_count, 2,
            "expected 2 VARIABLE symbols (imagesdir + source-dir)"
        );
    }

    /// Helper: flatten nested DocumentSymbol hierarchy into a flat list for easier assertions.
    fn symbols_flat(symbols: &[DocumentSymbol]) -> Vec<&DocumentSymbol> {
        let mut result = Vec::new();
        for s in symbols {
            result.push(s);
            if let Some(children) = &s.children {
                result.extend(symbols_flat(children));
            }
        }
        result
    }
}
