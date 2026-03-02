use std::path::Path;

use tower_lsp_server::ls_types::{CompletionItem, CompletionItemKind, Position};

use asciidoc_core::workspace::Workspace;

use crate::positions;

/// Classification of a completion trigger context.
#[derive(Debug, PartialEq)]
pub enum CompletionContext {
    /// `<<prefix` — cross-reference to an anchor
    Xref { prefix: String },
    /// `include::prefix` — include path completion
    IncludePath { prefix: String },
    /// `{prefix` — attribute reference (open brace, not yet closed)
    Attribute { prefix: String },
}

/// Detect the completion context at the given cursor position by scanning
/// the current line backwards from the cursor.
///
/// Priority: `include::` (longest prefix) > `xref:` > `<<` > `{`
pub fn detect_completion_context(content: &str, pos: Position) -> Option<CompletionContext> {
    let line_str = content.lines().nth(pos.line as usize)?;

    // Convert UTF-16 offset (LSP spec) to byte offset for safe string slicing
    let (_, byte_col) = positions::from_lsp_position(pos, content);
    let col = byte_col.min(line_str.len());
    let before_cursor = &line_str[..col];

    // Priority 1: include:: (longest trigger)
    if let Some(idx) = before_cursor.rfind("include::") {
        let prefix_start = idx + "include::".len();
        let prefix = &before_cursor[prefix_start..];
        // Check for open attribute reference inside include path:
        // If there's an unclosed `{` (no matching `}` after it), the user is
        // typing an attribute name — delegate to Attribute completion.
        if let Some(brace_pos) = prefix.rfind('{') {
            let after_brace = &prefix[brace_pos + 1..];
            if !after_brace.contains('}') {
                return Some(CompletionContext::Attribute {
                    prefix: after_brace.to_string(),
                });
            }
        }
        return Some(CompletionContext::IncludePath {
            prefix: prefix.to_string(),
        });
    }

    // Priority 2: xref: (long-form cross-reference)
    if let Some(idx) = before_cursor.rfind("xref:") {
        let prefix_start = idx + "xref:".len();
        let prefix = &before_cursor[prefix_start..];
        // If [ appears in prefix, the xref is closed — no completion
        if !prefix.contains('[') {
            // Check for open attribute reference inside xref target:
            // If there's an unclosed `{` (no matching `}` after it), the user is
            // typing an attribute name — delegate to Attribute completion.
            if let Some(brace_pos) = prefix.rfind('{') {
                let after_brace = &prefix[brace_pos + 1..];
                if !after_brace.contains('}') {
                    return Some(CompletionContext::Attribute {
                        prefix: after_brace.to_string(),
                    });
                }
            }
            return Some(CompletionContext::Xref {
                prefix: prefix.to_string(),
            });
        }
    }

    // Priority 3: << (xref shorthand)
    if let Some(idx) = before_cursor.rfind("<<") {
        let prefix_start = idx + "<<".len();
        let prefix = &before_cursor[prefix_start..];
        // If >> appears after <<, the xref is closed — no completion
        if !prefix.contains(">>") {
            return Some(CompletionContext::Xref {
                prefix: prefix.to_string(),
            });
        }
    }

    // Priority 4: { (attribute reference)
    if let Some(idx) = before_cursor.rfind('{') {
        let prefix_start = idx + 1;
        let prefix = &before_cursor[prefix_start..];
        // If } appears after {, the attribute is closed — no completion
        if !prefix.contains('}') && !prefix.contains(' ') {
            return Some(CompletionContext::Attribute {
                prefix: prefix.to_string(),
            });
        }
    }

    None
}

/// Resolve xref completions by listing all anchors from the workspace's
/// ReferenceIndex, filtered by prefix.
///
/// Returns `CompletionItem`s with:
/// - `label`: anchor ID
/// - `kind`: Reference
/// - `detail`: filename + anchor kind
/// - `insert_text`: `anchor_id>>` (appends closing `>>`)
pub fn resolve_xref_completions(prefix: &str, ws: Option<&Workspace>) -> Vec<CompletionItem> {
    let ws = match ws {
        Some(ws) => ws,
        None => return Vec::new(),
    };

    let mut items: Vec<CompletionItem> = ws
        .reference_index
        .anchors
        .iter()
        .filter(|(id, _)| id.starts_with(prefix))
        .map(|(id, loc)| {
            let filename = loc
                .file
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default();
            let kind_label = match loc.kind {
                asciidoc_core::types::AnchorKind::Explicit => "explicit",
                asciidoc_core::types::AnchorKind::ImplicitSection => "section",
                asciidoc_core::types::AnchorKind::BlockId => "block",
            };

            CompletionItem {
                label: id.clone(),
                kind: Some(CompletionItemKind::REFERENCE),
                detail: Some(format!("{} ({})", filename, kind_label)),
                insert_text: Some(format!("{}>>", id)),
                ..Default::default()
            }
        })
        .collect();

    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// Resolve include-path completions by listing `.adoc` files and subdirectories
/// relative to the document's parent directory, filtered by prefix.
///
/// Returns `CompletionItem`s with:
/// - `label`: filename or dirname (dirs end with `/`)
/// - `kind`: `File` for `.adoc` files, `Folder` for directories
/// - `insert_text`: relative path + `[]` for files, path + `/` for dirs
pub fn resolve_include_completions(
    prefix: &str,
    doc_path: &Path,
    ws: Option<&Workspace>,
) -> Vec<CompletionItem> {
    let base = doc_path.parent().unwrap_or(doc_path);

    // Attribute substitution: resolve {attr} references in prefix
    let resolved_prefix = match ws {
        Some(ws) => ws.attributes.substitute(doc_path, prefix),
        None => prefix.to_string(),
    };

    // Bail on unresolved attributes (still contains `{`)
    if resolved_prefix.contains('{') {
        return Vec::new();
    }

    // Split ORIGINAL prefix for insert_text, RESOLVED prefix for filesystem lookup
    let (original_dir_prefix, _) = match prefix.rfind('/') {
        Some(idx) => (&prefix[..=idx], &prefix[idx + 1..]),
        None => ("", prefix),
    };
    let (resolved_dir, filter) = match resolved_prefix.rfind('/') {
        Some(idx) => (&resolved_prefix[..=idx], &resolved_prefix[idx + 1..]),
        None => ("", resolved_prefix.as_str()),
    };

    let list_dir = base.join(resolved_dir);

    let entries = match std::fs::read_dir(&list_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut items: Vec<CompletionItem> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();

            if path.is_dir() {
                let dir_name = format!("{}/", name);
                if !dir_name.starts_with(filter) && !name.starts_with(filter) {
                    return None;
                }
                Some(CompletionItem {
                    label: dir_name.clone(),
                    kind: Some(CompletionItemKind::FOLDER),
                    insert_text: Some(format!("{}{}", original_dir_prefix, dir_name)),
                    ..Default::default()
                })
            } else if name.ends_with(".adoc") {
                if !name.starts_with(filter) {
                    return None;
                }
                Some(CompletionItem {
                    label: name.clone(),
                    kind: Some(CompletionItemKind::FILE),
                    insert_text: Some(format!("{}{}[]", original_dir_prefix, name)),
                    ..Default::default()
                })
            } else {
                None
            }
        })
        .collect();

    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// Resolve attribute completions by listing all attributes from the workspace's
/// AttributeRegistry, filtered by prefix.
///
/// Returns `CompletionItem`s with:
/// - `label`: attribute name
/// - `kind`: Variable
/// - `detail`: resolved attribute value
/// - `insert_text`: `attr_name}` (appends closing `}`)
pub fn resolve_attribute_completions(
    prefix: &str,
    doc_path: &Path,
    ws: Option<&Workspace>,
) -> Vec<CompletionItem> {
    let ws = match ws {
        Some(ws) => ws,
        None => return Vec::new(),
    };

    let mut items: Vec<CompletionItem> = ws
        .attributes
        .all_attributes(doc_path)
        .into_iter()
        .filter(|(name, _)| name.starts_with(prefix))
        .map(|(name, value)| CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::VARIABLE),
            detail: Some(value),
            insert_text: Some(format!("{}}}", name)),
            ..Default::default()
        })
        .collect();

    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// Dispatch completion resolution based on the detected context.
///
/// Routes to the appropriate resolve function:
/// - `Xref` → `resolve_xref_completions`
/// - `IncludePath` → `resolve_include_completions` (uses `doc_path` for relative listing)
/// - `Attribute` → `resolve_attribute_completions`
pub fn resolve_completions(
    ctx: CompletionContext,
    doc_path: &Path,
    ws: Option<&Workspace>,
) -> Vec<CompletionItem> {
    match ctx {
        CompletionContext::Xref { prefix } => resolve_xref_completions(&prefix, ws),
        CompletionContext::IncludePath { prefix } => {
            resolve_include_completions(&prefix, doc_path, ws)
        }
        CompletionContext::Attribute { prefix } => {
            resolve_attribute_completions(&prefix, doc_path, ws)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_xref_trigger() {
        let content = "See <<my-pre";
        let pos = Position {
            line: 0,
            character: 12,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::Xref {
                prefix: "my-pre".into()
            })
        );
    }

    #[test]
    fn test_detect_include_trigger() {
        let content = "include::chap";
        let pos = Position {
            line: 0,
            character: 13,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::IncludePath {
                prefix: "chap".into()
            })
        );
    }

    #[test]
    fn test_detect_attribute_trigger() {
        let content = "The {img";
        let pos = Position {
            line: 0,
            character: 8,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::Attribute {
                prefix: "img".into()
            })
        );
    }

    #[test]
    fn test_detect_no_trigger() {
        let content = "Just normal text here.";
        let pos = Position {
            line: 0,
            character: 10,
        };
        assert_eq!(detect_completion_context(content, pos), None);
    }

    #[test]
    fn test_detect_closed_attribute() {
        let content = "The {imagesdir} is set.";
        let pos = Position {
            line: 0,
            character: 15,
        };
        assert_eq!(detect_completion_context(content, pos), None);
    }

    #[test]
    fn test_detect_xref_empty_prefix() {
        let content = "See <<";
        let pos = Position {
            line: 0,
            character: 6,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::Xref { prefix: "".into() })
        );
    }

    #[test]
    fn test_detect_multiline() {
        let content = "= Title\n\nSee <<my-id";
        let pos = Position {
            line: 2,
            character: 11, // end of "See <<my-id" (len=11)
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::Xref {
                prefix: "my-id".into()
            })
        );
    }

    #[test]
    fn test_detect_xref_with_umlaut_prefix() {
        // "Über" contains ü (U+00FC): 2 bytes UTF-8, 1 code unit UTF-16
        // Line: "Über <<anch" — ü is at byte 0..2, so "<<" starts at byte 6
        // UTF-16 positions: Ü=1, b=2, e=3, r=4, ' '=5, <=6, <=7, a=8, n=9, c=10, h=11
        let content = "Über <<anch";
        let pos = Position {
            line: 0,
            character: 11, // UTF-16 offset: end of "anch"
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::Xref {
                prefix: "anch".into()
            })
        );
    }

    #[test]
    fn test_detect_attribute_with_cjk() {
        // CJK character 中 (U+4E2D): 3 bytes UTF-8, 1 code unit UTF-16
        // Line: "中{attr" — 中 is at byte 0..3, { at byte 3, "attr" at byte 4..8
        // UTF-16 positions: 中=1, {=2, a=3, t=4, t=5, r=6
        let content = "中{attr";
        let pos = Position {
            line: 0,
            character: 6, // UTF-16 offset: end of "attr"
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::Attribute {
                prefix: "attr".into()
            })
        );
    }

    #[test]
    fn test_include_priority_over_attribute() {
        // include:: contains a { but attribute is closed — should detect as include
        let content = "include::{docdir}/chap";
        let pos = Position {
            line: 0,
            character: 22,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::IncludePath {
                prefix: "{docdir}/chap".into()
            })
        );
    }

    #[test]
    fn test_include_open_brace_triggers_attribute() {
        // include::{snip — open brace without closing } → attribute completion
        let content = "include::{snip";
        let pos = Position {
            line: 0,
            character: 14,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::Attribute {
                prefix: "snip".into()
            })
        );
    }

    #[test]
    fn test_include_closed_brace_stays_include() {
        // include::{snippetsdir}/file — attribute is closed → include path completion
        let content = "include::{snippetsdir}/file";
        let pos = Position {
            line: 0,
            character: 27,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::IncludePath {
                prefix: "{snippetsdir}/file".into()
            })
        );
    }

    #[test]
    fn test_detect_xref_long_form() {
        // xref:sec → Xref { prefix: "sec" }
        let content = "See xref:sec";
        let pos = Position {
            line: 0,
            character: 12,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::Xref {
                prefix: "sec".into()
            })
        );
    }

    #[test]
    fn test_detect_xref_long_form_closed() {
        // xref:target[Link Text → None (closed by [)
        let content = "See xref:target[Link Text";
        let pos = Position {
            line: 0,
            character: 25,
        };
        assert_eq!(detect_completion_context(content, pos), None);
    }

    #[test]
    fn test_detect_xref_long_form_open_attribute() {
        // xref:{attr → Attribute { prefix: "attr" } (open brace)
        let content = "See xref:{attr";
        let pos = Position {
            line: 0,
            character: 14,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::Attribute {
                prefix: "attr".into()
            })
        );
    }

    #[test]
    fn test_detect_xref_long_form_closed_attribute() {
        // xref:{resolved}/sec → Xref { prefix: "{resolved}/sec" } (closed brace)
        let content = "See xref:{resolved}/sec";
        let pos = Position {
            line: 0,
            character: 23,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::Xref {
                prefix: "{resolved}/sec".into()
            })
        );
    }

    // --- Phase 2: resolve_xref_completions ---

    use asciidoc_core::attributes::AttributeRegistry;
    use asciidoc_core::graph::IncludeGraph;
    use asciidoc_core::references::ReferenceIndex;
    use asciidoc_core::types::{AnchorKind, AnchorLocation};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    /// Helper: build a minimal Workspace with a pre-built ReferenceIndex.
    fn ws_with_anchors(anchors: Vec<(&str, AnchorLocation)>) -> Workspace {
        let mut index = ReferenceIndex::new();
        for (id, loc) in anchors {
            index.add_anchor(id, loc);
        }
        Workspace {
            root: PathBuf::from("/tmp"),
            attributes: AttributeRegistry::new(Path::new("/tmp")),
            include_graph: IncludeGraph::new(),
            reference_index: index,
            documents: HashMap::new(),
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn test_xref_completions_basic() {
        let ws = ws_with_anchors(vec![
            (
                "my-anchor",
                AnchorLocation {
                    file: PathBuf::from("/a.adoc"),
                    line: 5,
                    kind: AnchorKind::Explicit,
                },
            ),
            (
                "my-section",
                AnchorLocation {
                    file: PathBuf::from("/a.adoc"),
                    line: 10,
                    kind: AnchorKind::ImplicitSection,
                },
            ),
            (
                "other-id",
                AnchorLocation {
                    file: PathBuf::from("/b.adoc"),
                    line: 1,
                    kind: AnchorKind::Explicit,
                },
            ),
        ]);

        let items = resolve_xref_completions("my", Some(&ws));
        assert_eq!(items.len(), 2);
        // Sorted alphabetically
        assert_eq!(items[0].label, "my-anchor");
        assert_eq!(items[1].label, "my-section");
        // Kind
        assert_eq!(items[0].kind, Some(CompletionItemKind::REFERENCE));
        // Detail contains filename + kind
        assert!(items[0].detail.as_ref().unwrap().contains("a.adoc"));
        assert!(items[0].detail.as_ref().unwrap().contains("explicit"));
        assert!(items[1].detail.as_ref().unwrap().contains("section"));
        // insert_text appends >>
        assert_eq!(items[0].insert_text.as_ref().unwrap(), "my-anchor>>");
        assert_eq!(items[1].insert_text.as_ref().unwrap(), "my-section>>");
    }

    #[test]
    fn test_xref_completions_empty_prefix() {
        let ws = ws_with_anchors(vec![
            (
                "alpha",
                AnchorLocation {
                    file: PathBuf::from("/a.adoc"),
                    line: 1,
                    kind: AnchorKind::Explicit,
                },
            ),
            (
                "beta",
                AnchorLocation {
                    file: PathBuf::from("/b.adoc"),
                    line: 2,
                    kind: AnchorKind::ImplicitSection,
                },
            ),
        ]);

        let items = resolve_xref_completions("", Some(&ws));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "alpha");
        assert_eq!(items[1].label, "beta");
    }

    #[test]
    fn test_xref_completions_no_workspace() {
        let items = resolve_xref_completions("my", None);
        assert!(items.is_empty());
    }

    // --- Phase 3: resolve_include_completions ---

    use tempfile::TempDir;

    /// Helper: create a tempdir with .adoc files and subdirectories for include tests.
    fn setup_include_dir() -> TempDir {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.adoc"), "= Doc A\n").unwrap();
        std::fs::write(dir.path().join("b.adoc"), "= Doc B\n").unwrap();
        std::fs::create_dir(dir.path().join("chapters")).unwrap();
        std::fs::write(dir.path().join("chapters/intro.adoc"), "= Intro\n").unwrap();
        dir
    }

    #[test]
    fn test_include_completions_files() {
        let dir = setup_include_dir();
        let doc_file = dir.path().join("dummy.adoc");
        let items = resolve_include_completions("", &doc_file, None);
        // Should contain a.adoc, b.adoc, and chapters/
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"a.adoc"), "missing a.adoc in {:?}", labels);
        assert!(labels.contains(&"b.adoc"), "missing b.adoc in {:?}", labels);
        assert!(
            labels.contains(&"chapters/"),
            "missing chapters/ in {:?}",
            labels
        );
        // Files have File kind
        let a_item = items.iter().find(|i| i.label == "a.adoc").unwrap();
        assert_eq!(a_item.kind, Some(CompletionItemKind::FILE));
        // insert_text for files appends []
        assert_eq!(a_item.insert_text.as_ref().unwrap(), "a.adoc[]");
        // Directories have Folder kind
        let ch_item = items.iter().find(|i| i.label == "chapters/").unwrap();
        assert_eq!(ch_item.kind, Some(CompletionItemKind::FOLDER));
        // insert_text for dirs appends /  (already in label, no extra suffix)
        assert_eq!(ch_item.insert_text.as_ref().unwrap(), "chapters/");
    }

    #[test]
    fn test_include_completions_prefix_filter() {
        let dir = setup_include_dir();
        let doc_file = dir.path().join("dummy.adoc");
        let items = resolve_include_completions("chap", &doc_file, None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "chapters/");
    }

    #[test]
    fn test_include_completions_subdir() {
        let dir = setup_include_dir();
        let doc_file = dir.path().join("dummy.adoc");
        let items = resolve_include_completions("chapters/", &doc_file, None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "intro.adoc");
        assert_eq!(
            items[0].insert_text.as_ref().unwrap(),
            "chapters/intro.adoc[]"
        );
    }

    // --- Phase 3b: resolve_include_completions with attribute substitution ---

    /// Helper: create a tempdir with .asciidoctorconfig, src/ subdir with .adoc files,
    /// and a doc file at root level. Returns (TempDir, Workspace, PathBuf of doc_file).
    fn setup_include_attr_dir() -> (TempDir, Workspace, PathBuf) {
        let dir = TempDir::new().unwrap();
        // Config: :source-dir: src
        std::fs::write(dir.path().join(".asciidoctorconfig"), ":source-dir: src\n").unwrap();
        // Create src/ with .adoc files
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/module-a.adoc"), "= Module A\n").unwrap();
        std::fs::write(dir.path().join("src/module-b.adoc"), "= Module B\n").unwrap();
        // Doc file at root
        let doc_file = dir.path().join("index.adoc");
        std::fs::write(&doc_file, "= Index\n").unwrap();

        let mut attrs = AttributeRegistry::new(dir.path());
        attrs.load_config(dir.path());

        let ws = Workspace {
            root: dir.path().to_path_buf(),
            attributes: attrs,
            include_graph: IncludeGraph::new(),
            reference_index: ReferenceIndex::new(),
            documents: HashMap::new(),
            diagnostics: Vec::new(),
        };
        (dir, ws, doc_file)
    }

    #[test]
    fn test_include_completions_with_resolved_attr() {
        let (_dir, ws, doc_file) = setup_include_attr_dir();
        let items = resolve_include_completions("{source-dir}/", &doc_file, Some(&ws));
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"module-a.adoc"),
            "missing module-a.adoc in {:?}",
            labels
        );
        assert!(
            labels.contains(&"module-b.adoc"),
            "missing module-b.adoc in {:?}",
            labels
        );
        // insert_text preserves original {source-dir}/ prefix
        let a_item = items.iter().find(|i| i.label == "module-a.adoc").unwrap();
        assert_eq!(
            a_item.insert_text.as_ref().unwrap(),
            "{source-dir}/module-a.adoc[]"
        );
    }

    #[test]
    fn test_include_completions_with_unresolved_attr() {
        let (_dir, ws, doc_file) = setup_include_attr_dir();
        let items = resolve_include_completions("{unknown}/", &doc_file, Some(&ws));
        assert!(items.is_empty(), "expected empty, got {:?}", items);
    }

    #[test]
    fn test_include_completions_attr_with_filter() {
        let (_dir, ws, doc_file) = setup_include_attr_dir();
        let items = resolve_include_completions("{source-dir}/module-a", &doc_file, Some(&ws));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "module-a.adoc");
        assert_eq!(
            items[0].insert_text.as_ref().unwrap(),
            "{source-dir}/module-a.adoc[]"
        );
    }

    #[test]
    fn test_include_completions_no_attr_with_workspace() {
        let (_dir, ws, doc_file) = setup_include_attr_dir();
        // No attribute in prefix — should work like before, even with workspace present
        let items = resolve_include_completions("src/", &doc_file, Some(&ws));
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"module-a.adoc"),
            "missing module-a.adoc in {:?}",
            labels
        );
        assert!(
            labels.contains(&"module-b.adoc"),
            "missing module-b.adoc in {:?}",
            labels
        );
    }

    // --- Phase 4: resolve_attribute_completions ---

    use asciidoc_core::types::AttributeDef;

    /// Helper: build a Workspace with config (.asciidoctorconfig) + per-file attributes.
    fn ws_with_config_and_file_attrs(
        config_content: &str,
        file: &Path,
        file_attrs: Vec<AttributeDef>,
    ) -> (TempDir, Workspace) {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".asciidoctorconfig"), config_content).unwrap();

        let mut attrs = AttributeRegistry::new(dir.path());
        attrs.load_config(dir.path());
        attrs.register_file_attributes(file, &file_attrs);

        let ws = Workspace {
            root: dir.path().to_path_buf(),
            attributes: attrs,
            include_graph: IncludeGraph::new(),
            reference_index: ReferenceIndex::new(),
            documents: HashMap::new(),
            diagnostics: Vec::new(),
        };
        (dir, ws)
    }

    #[test]
    fn test_attribute_completions_basic() {
        let file = PathBuf::from("/tmp/doc.adoc");
        let (_dir, ws) = ws_with_config_and_file_attrs(
            ":imagesdir: images\n",
            &file,
            vec![AttributeDef {
                name: "author".into(),
                value: "Alice".into(),
                line: 2,
            }],
        );

        let items = resolve_attribute_completions("", &file, Some(&ws));
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        // Builtin
        assert!(labels.contains(&"docdir"), "missing docdir in {:?}", labels);
        // Config
        assert!(
            labels.contains(&"imagesdir"),
            "missing imagesdir in {:?}",
            labels
        );
        // Per-file
        assert!(labels.contains(&"author"), "missing author in {:?}", labels);

        // Check kind = Variable
        let author = items.iter().find(|i| i.label == "author").unwrap();
        assert_eq!(author.kind, Some(CompletionItemKind::VARIABLE));
        // Check detail = resolved value
        assert_eq!(author.detail.as_ref().unwrap(), "Alice");
        // Check insert_text appends closing }
        assert_eq!(author.insert_text.as_ref().unwrap(), "author}");
    }

    #[test]
    fn test_attribute_completions_prefix_filter() {
        let file = PathBuf::from("/tmp/doc.adoc");
        let (_dir, ws) = ws_with_config_and_file_attrs(
            ":imagesdir: images\n",
            &file,
            vec![AttributeDef {
                name: "author".into(),
                value: "Alice".into(),
                line: 2,
            }],
        );

        let items = resolve_attribute_completions("im", &file, Some(&ws));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "imagesdir");
    }

    #[test]
    fn test_attribute_completions_no_workspace() {
        let file = Path::new("/tmp/doc.adoc");
        let items = resolve_attribute_completions("", file, None);
        assert!(items.is_empty());
    }

    // --- Phase 5: resolve_completions (dispatcher) ---

    #[test]
    fn test_resolve_completions_xref() {
        let ws = ws_with_anchors(vec![(
            "my-anchor",
            AnchorLocation {
                file: PathBuf::from("/a.adoc"),
                line: 5,
                kind: AnchorKind::Explicit,
            },
        )]);
        let doc_path = Path::new("/a.adoc");
        let ctx = CompletionContext::Xref {
            prefix: "my".into(),
        };

        let items = resolve_completions(ctx, doc_path, Some(&ws));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "my-anchor");
    }

    #[test]
    fn test_resolve_completions_include() {
        let dir = setup_include_dir();
        // doc_path is a file inside the dir (as in real LSP usage)
        let doc_file = dir.path().join("a.adoc");
        let ctx = CompletionContext::IncludePath { prefix: "".into() };

        let items = resolve_completions(ctx, &doc_file, None);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"a.adoc"));
        assert!(labels.contains(&"b.adoc"));
        assert!(labels.contains(&"chapters/"));
    }

    #[test]
    fn test_resolve_completions_attribute() {
        let file = PathBuf::from("/tmp/doc.adoc");
        let (_dir, ws) = ws_with_config_and_file_attrs(
            ":imagesdir: images\n",
            &file,
            vec![AttributeDef {
                name: "author".into(),
                value: "Alice".into(),
                line: 2,
            }],
        );
        let ctx = CompletionContext::Attribute {
            prefix: "im".into(),
        };

        let items = resolve_completions(ctx, &file, Some(&ws));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "imagesdir");
    }
}
