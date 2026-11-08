use std::path::Path;

use tower_lsp_server::ls_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, InsertTextFormat, Position, Range,
    TextEdit,
};

use asciidoc_core::workspace::Workspace;

use crate::positions;

// ---------------------------------------------------------------------------
// Syntax Completion Catalog
// ---------------------------------------------------------------------------

/// A static catalog entry for `AsciiDoc` syntax completions.
struct SyntaxEntry {
    name: &'static str,
    snippet: &'static str,
    detail: &'static str,
}

/// Block macros — triggered at line start (e.g. `ima` → `image::path[alt]`).
const BLOCK_MACROS: &[SyntaxEntry] = &[
    SyntaxEntry {
        name: "include::",
        snippet: "include::${1:path}[${2:opts}]",
        detail: "Include another document",
    },
    SyntaxEntry {
        name: "image::",
        snippet: "image::${1:path}[${2:alt}]",
        detail: "Block image",
    },
    SyntaxEntry {
        name: "video::",
        snippet: "video::${1:path}[${2:opts}]",
        detail: "Block video",
    },
    SyntaxEntry {
        name: "audio::",
        snippet: "audio::${1:path}[]",
        detail: "Block audio",
    },
    SyntaxEntry {
        name: "toc::",
        snippet: "toc::[]",
        detail: "Table of contents macro",
    },
    SyntaxEntry {
        name: "ifdef::",
        snippet: "ifdef::${1:attribute}[${2:content}]",
        detail: "Conditional content (attribute defined)",
    },
    SyntaxEntry {
        name: "ifndef::",
        snippet: "ifndef::${1:attribute}[${2:content}]",
        detail: "Conditional content (attribute not defined)",
    },
    SyntaxEntry {
        name: "endif::",
        snippet: "endif::${1:attribute}[]",
        detail: "End conditional block",
    },
    SyntaxEntry {
        name: "ifeval::",
        snippet: "ifeval::[${1:expression}]",
        detail: "Conditional content (expression evaluation)",
    },
];

/// Admonitions — triggered at line start with uppercase (e.g. `NO` → `NOTE: `).
const ADMONITIONS: &[SyntaxEntry] = &[
    SyntaxEntry {
        name: "NOTE:",
        snippet: "NOTE: $0",
        detail: "Note admonition",
    },
    SyntaxEntry {
        name: "TIP:",
        snippet: "TIP: $0",
        detail: "Tip admonition",
    },
    SyntaxEntry {
        name: "IMPORTANT:",
        snippet: "IMPORTANT: $0",
        detail: "Important admonition",
    },
    SyntaxEntry {
        name: "WARNING:",
        snippet: "WARNING: $0",
        detail: "Warning admonition",
    },
    SyntaxEntry {
        name: "CAUTION:",
        snippet: "CAUTION: $0",
        detail: "Caution admonition",
    },
];

/// Document attributes — triggered after `:` at line start (e.g. `:to` → `:toc:`).
/// `snippet` does NOT include the leading `:` (already typed).
const DOC_ATTRIBUTES: &[SyntaxEntry] = &[
    SyntaxEntry {
        name: "toc",
        snippet: "toc: ${1|auto,left,right,preamble|}",
        detail: "Table of contents placement",
    },
    SyntaxEntry {
        name: "icons",
        snippet: "icons: ${1|font,image|}",
        detail: "Icon mode",
    },
    SyntaxEntry {
        name: "source-highlighter",
        snippet: "source-highlighter: ${1|rouge,highlight.js,coderay|}",
        detail: "Source code highlighter",
    },
    SyntaxEntry {
        name: "doctype",
        snippet: "doctype: ${1|article,book,manpage,inline|}",
        detail: "Document type",
    },
    SyntaxEntry {
        name: "imagesdir",
        snippet: "imagesdir: ${1:images}",
        detail: "Images directory",
    },
    SyntaxEntry {
        name: "numbered",
        snippet: "numbered:",
        detail: "Auto-number sections (deprecated, use sectnums)",
    },
    SyntaxEntry {
        name: "experimental",
        snippet: "experimental:",
        detail: "Enable experimental features (kbd, btn, menu)",
    },
    SyntaxEntry {
        name: "data-uri",
        snippet: "data-uri:",
        detail: "Embed images as data URIs",
    },
    SyntaxEntry {
        name: "sectnums",
        snippet: "sectnums:",
        detail: "Number sections",
    },
    SyntaxEntry {
        name: "sectanchors",
        snippet: "sectanchors:",
        detail: "Add anchors to section titles",
    },
    SyntaxEntry {
        name: "sectlinks",
        snippet: "sectlinks:",
        detail: "Make section titles self-links",
    },
    SyntaxEntry {
        name: "stem",
        snippet: "stem: ${1|latexmath,asciimath|}",
        detail: "Enable STEM (math) support",
    },
    SyntaxEntry {
        name: "toclevels",
        snippet: "toclevels: ${1:2}",
        detail: "Number of ToC depth levels",
    },
    SyntaxEntry {
        name: "hardbreaks-option",
        snippet: "hardbreaks-option:",
        detail: "Force hard line breaks",
    },
];

/// Inline macros — triggered after known macro name + `:` (e.g. `kbd:` → `[keys]`).
/// `snippet` contains only what comes AFTER the `:` (macro name already typed).
const INLINE_MACROS: &[SyntaxEntry] = &[
    SyntaxEntry {
        name: "kbd",
        snippet: "[${1:keys}]",
        detail: "Keyboard shortcut",
    },
    SyntaxEntry {
        name: "btn",
        snippet: "[${1:label}]",
        detail: "UI button",
    },
    SyntaxEntry {
        name: "menu",
        snippet: "${1:menu}[${2:items}]",
        detail: "Menu selection",
    },
    SyntaxEntry {
        name: "link",
        snippet: "${1:url}[${2:text}]",
        detail: "Inline link",
    },
    SyntaxEntry {
        name: "image",
        snippet: "${1:path}[${2:alt}]",
        detail: "Inline image",
    },
    SyntaxEntry {
        name: "footnote",
        snippet: "[${1:text}]",
        detail: "Footnote",
    },
    SyntaxEntry {
        name: "pass",
        snippet: "[${1:content}]",
        detail: "Passthrough",
    },
];

// ---------------------------------------------------------------------------
// Completion Context
// ---------------------------------------------------------------------------

/// Range info for an existing `[…]` block immediately after the cursor.
/// When present, anchor/path resolvers emit a `TextEdit` that swallows the
/// existing brackets and re-emits them as a snippet `[${1:label}]`, so the
/// `label` placeholder is selected after the user accepts the completion.
#[derive(Debug, PartialEq, Clone, Copy)]
pub(crate) struct TrailingBrackets {
    pub line: u32,
    pub cursor_col: u32, // UTF-16 column of cursor
    pub end_col: u32,    // UTF-16 column AFTER closing `]`
}

/// Classification of a completion trigger context.
#[derive(Debug, PartialEq)]
pub(crate) enum CompletionContext {
    /// `<<prefix` — cross-reference anchor (shorthand). `closing` is the suffix
    /// appended to `insert_text` (`>>` for `<<…>>`, `[]` for `xref:…[]`).
    XrefAnchor {
        prefix: String,
        closing: &'static str,
        trailing: Option<TrailingBrackets>,
    },
    /// `xref:prefix` without `#` — could be either a workspace anchor or an
    /// `.adoc` file path. Resolver dispatches to *both* and merges results to
    /// support discovery. `closing` is `"[]"` for fresh edits, or `""` when the
    /// brackets are already present after the cursor (avoid doubling).
    XrefAnchorOrPath {
        prefix: String,
        closing: &'static str,
        trailing: Option<TrailingBrackets>,
    },
    /// `xref:file.adoc#anchor_prefix` or `<<file.adoc#anchor_prefix` —
    /// cross-file anchor. Empty `file` means same-document (e.g. `xref:#sec`).
    XrefCrossAnchor {
        file: String,
        anchor_prefix: String,
        closing: &'static str,
        trailing: Option<TrailingBrackets>,
    },
    /// `include::prefix` — include path completion
    IncludePath { prefix: String },
    /// `image::prefix` — image path completion
    ImagePath { prefix: String },
    /// `{prefix` — attribute reference (open brace, not yet closed)
    Attribute { prefix: String },
    /// `ima` at line start — block macro completion
    BlockMacro { prefix: String },
    /// `NO` at line start — admonition completion
    Admonition { prefix: String },
    /// `:to` at line start — document attribute completion
    DocumentAttribute { prefix: String },
    /// `kbd` mid-line or at line start — inline macro prefix (no `:` typed yet)
    InlineMacroPrefix {
        prefix: String,
        line: u32,
        word_start_col: u32,
        cursor_col: u32,
    },
    /// `kbd:` — inline macro completion (after known macro name + `:`)
    InlineMacro { macro_name: String },
}

/// Detect the completion context at the given cursor position by scanning
/// the current line backwards from the cursor.
///
/// Priority: `include::` (longest prefix) > `xref:` > `<<` > `{`
pub(crate) fn detect_completion_context(content: &str, pos: Position) -> Option<CompletionContext> {
    let line_str = content.lines().nth(pos.line as usize)?;

    // Convert UTF-16 offset (LSP spec) to byte offset for safe string slicing
    let (_, byte_col) = positions::from_lsp_position(pos, content);
    let col = byte_col.min(line_str.len());
    let before_cursor = &line_str[..col];
    let after_cursor = &line_str[col..];

    // When the user is mid-edit (e.g. accepted a path-completion snippet that
    // pre-inserted `[label]` and is now typing `#anchor` before it), the line
    // after the cursor already contains the closing brackets. Re-emitting `[]`
    // or `>>` would double them. Compute effective closings up-front.
    let xref_long_close = if after_cursor.starts_with('[') {
        ""
    } else {
        "[]"
    };
    let xref_short_close = if after_cursor.starts_with(">>") {
        ""
    } else {
        ">>"
    };

    // Detect an existing `[…]` block right after the cursor and compute UTF-16
    // columns so resolvers can emit a TextEdit that overwrites the existing
    // label and selects a fresh `${1:label}` placeholder. Only meaningful for
    // long-form xref (`[]` closing was suppressed to "" above).
    let xref_trailing: Option<TrailingBrackets> =
        after_cursor.strip_prefix('[').and_then(|after_open| {
            after_open.find(']').map(|rel_idx| {
                let end_byte = rel_idx + 2; // `[` + content + `]`
                #[allow(clippy::cast_possible_truncation)]
                let utf16_len = after_cursor[..end_byte].encode_utf16().count() as u32;
                TrailingBrackets {
                    line: pos.line,
                    cursor_col: pos.character,
                    end_col: pos.character + utf16_len,
                }
            })
        });

    // Priority 1a: include:: (longest trigger)
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

    // Priority 1b: image:: (block image path)
    if let Some(idx) = before_cursor.rfind("image::") {
        let prefix_start = idx + "image::".len();
        let prefix = &before_cursor[prefix_start..];
        // If [ appears, the macro is already closed — no completion
        if !prefix.contains('[') {
            if let Some(brace_pos) = prefix.rfind('{') {
                let after_brace = &prefix[brace_pos + 1..];
                if !after_brace.contains('}') {
                    return Some(CompletionContext::Attribute {
                        prefix: after_brace.to_string(),
                    });
                }
            }
            return Some(CompletionContext::ImagePath {
                prefix: prefix.to_string(),
            });
        }
    }

    // Priority 1c: image: (inline image path — single colon)
    // Must come after 1b so that image:: is matched first.
    if let Some(idx) = before_cursor.rfind("image:") {
        let after_match = &before_cursor[idx + "image:".len()..];
        // Guard: if the char right after "image:" is ':', this is image:: (Priority 1b)
        if !after_match.starts_with(':') {
            let prefix = after_match;
            if !prefix.contains('[') {
                if let Some(brace_pos) = prefix.rfind('{') {
                    let after_brace = &prefix[brace_pos + 1..];
                    if !after_brace.contains('}') {
                        return Some(CompletionContext::Attribute {
                            prefix: after_brace.to_string(),
                        });
                    }
                }
                return Some(CompletionContext::ImagePath {
                    prefix: prefix.to_string(),
                });
            }
        }
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
            // Split at `#` for cross-file anchor form `xref:file.adoc#anchor`.
            if let Some(hash_pos) = prefix.find('#') {
                return Some(CompletionContext::XrefCrossAnchor {
                    file: prefix[..hash_pos].to_string(),
                    anchor_prefix: prefix[hash_pos + 1..].to_string(),
                    closing: xref_long_close,
                    trailing: xref_trailing,
                });
            }
            return Some(CompletionContext::XrefAnchorOrPath {
                prefix: prefix.to_string(),
                closing: xref_long_close,
                trailing: xref_trailing,
            });
        }
    }

    // Priority 3: << (xref shorthand)
    if let Some(idx) = before_cursor.rfind("<<") {
        let prefix_start = idx + "<<".len();
        let prefix = &before_cursor[prefix_start..];
        // If >> appears after <<, the xref is closed — no completion
        if !prefix.contains(">>") {
            // Cross-file anchor form `<<file.adoc#anchor>>`
            if let Some(hash_pos) = prefix.find('#') {
                return Some(CompletionContext::XrefCrossAnchor {
                    file: prefix[..hash_pos].to_string(),
                    anchor_prefix: prefix[hash_pos + 1..].to_string(),
                    closing: xref_short_close,
                    // Shorthand uses `>>` not `[]`, so trailing-bracket logic
                    // doesn't apply; existing tests assume None.
                    trailing: None,
                });
            }
            return Some(CompletionContext::XrefAnchor {
                prefix: prefix.to_string(),
                closing: xref_short_close,
                trailing: None,
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

    // Priority 5: DocumentAttribute — `:prefix` at line start, no closing `:`
    if let Some(rest) = before_cursor.strip_prefix(':') {
        if !rest.contains(':')
            && rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Some(CompletionContext::DocumentAttribute {
                prefix: rest.to_string(),
            });
        }
    }

    // Priority 6: BlockMacro — only lowercase at line start, AND must match catalog
    if !before_cursor.is_empty()
        && before_cursor == before_cursor.trim()
        && before_cursor.chars().all(|c| c.is_ascii_lowercase())
        && BLOCK_MACROS
            .iter()
            .any(|e| e.name.starts_with(before_cursor))
    {
        return Some(CompletionContext::BlockMacro {
            prefix: before_cursor.to_string(),
        });
    }

    // Priority 7: Admonition — uppercase at line start (optional trailing `:`)
    if !before_cursor.is_empty() && before_cursor == before_cursor.trim() {
        let trimmed = before_cursor.trim_end_matches(':');
        if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_uppercase()) {
            return Some(CompletionContext::Admonition {
                prefix: trimmed.to_string(),
            });
        }
    }

    // Priority 8: InlineMacroPrefix — known inline macro name at word boundary (no `:` yet)
    {
        let word_start = before_cursor
            .rfind(|c: char| !c.is_ascii_alphanumeric())
            .map_or(0, |i| {
                i + before_cursor[i..].chars().next().map_or(1, char::len_utf8)
            });
        let word = &before_cursor[word_start..];
        if !word.is_empty() && INLINE_MACROS.iter().any(|e| e.name.starts_with(word)) {
            // LSP uses u32 for column offsets; real-world lines won't exceed u32::MAX.
            #[allow(clippy::cast_possible_truncation)]
            let word_start_col = line_str[..word_start].encode_utf16().count() as u32;
            return Some(CompletionContext::InlineMacroPrefix {
                prefix: word.to_string(),
                line: pos.line,
                word_start_col,
                cursor_col: pos.character,
            });
        }
    }

    // Priority 9: InlineMacro — known macro name before `:`
    if let Some(colon_pos) = before_cursor.rfind(':') {
        let after_colon = &before_cursor[colon_pos + 1..];
        // Nothing after `:`, and not `::` (block macro)
        if after_colon.is_empty() && !before_cursor.ends_with("::") {
            // Extract word before `:`
            let before_colon = &before_cursor[..colon_pos];
            let word_start = before_colon
                .rfind(|c: char| !c.is_ascii_alphanumeric())
                .map_or(0, |i| {
                    i + before_colon[i..].chars().next().map_or(1, char::len_utf8)
                });
            let word = &before_colon[word_start..];
            if !word.is_empty() && INLINE_MACROS.iter().any(|e| e.name == word) {
                return Some(CompletionContext::InlineMacro {
                    macro_name: word.to_string(),
                });
            }
        }
    }

    None
}

/// Resolve xref anchor completions by listing all anchors from the workspace's
/// `ReferenceIndex`, filtered by prefix. `closing` (`">>"` or `"[]"`) is
/// appended to `insert_text`. When `trailing` is `Some`, an existing `[…]` is
/// overwritten with a snippet so the `label` placeholder ends up selected.
pub(crate) fn resolve_xref_completions(
    prefix: &str,
    ws: Option<&Workspace>,
    closing: &str,
    trailing: Option<TrailingBrackets>,
) -> Vec<CompletionItem> {
    let Some(ws) = ws else {
        return Vec::new();
    };

    let mut items: Vec<CompletionItem> = ws
        .reference_index()
        .anchors()
        .iter()
        .filter(|(id, _)| id.starts_with(prefix))
        .map(|(id, loc)| anchor_completion_item(id, loc, closing, trailing))
        .collect();

    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// Resolve cross-file xref anchor completions: anchors filtered to a specific
/// target file. Empty `file` means the current document.
///
/// File matching is layered to mirror `references::resolve_xref` semantics:
///   1. Canonical-path equality (handles `../foo.adoc`, symlinks, when target exists)
///   2. Path-suffix match (`subdir/foo.adoc` ends-with comparison on stored anchor path)
///   3. Filename-only fallback (any indexed file with matching basename)
pub(crate) fn resolve_xref_cross_anchor_completions(
    file: &str,
    anchor_prefix: &str,
    doc_path: &Path,
    ws: Option<&Workspace>,
    closing: &str,
    trailing: Option<TrailingBrackets>,
) -> Vec<CompletionItem> {
    let Some(ws) = ws else {
        return Vec::new();
    };

    if file.is_empty() {
        let doc_canonical =
            std::fs::canonicalize(doc_path).unwrap_or_else(|_| doc_path.to_path_buf());
        let mut items: Vec<CompletionItem> = ws
            .reference_index()
            .anchors()
            .iter()
            .filter(|(id, loc)| {
                if !id.starts_with(anchor_prefix) {
                    return false;
                }
                let loc_canonical =
                    std::fs::canonicalize(&loc.file).unwrap_or_else(|_| loc.file.clone());
                loc_canonical == doc_canonical
            })
            .map(|(id, loc)| anchor_completion_item(id, loc, closing, trailing))
            .collect();
        items.sort_by(|a, b| a.label.cmp(&b.label));
        return items;
    }

    let resolved = ws.attributes().substitute(doc_path, file);
    if resolved.contains('{') {
        return Vec::new();
    }

    // Only keep canonical (existing-on-disk) results — downstream compares
    // against `std::fs::canonicalize(&loc.file)` for equality.
    let canonical_target =
        asciidoc_core::paths::resolve_doc_target(doc_path, file, ws.attributes())
            .and_then(|p| std::fs::canonicalize(p).ok());

    let resolved_path = Path::new(&resolved);
    let resolved_filename = resolved_path
        .file_name()
        .map(|f| f.to_string_lossy().to_string());

    let mut items: Vec<CompletionItem> = ws
        .reference_index()
        .anchors()
        .iter()
        .filter(|(id, loc)| {
            if !id.starts_with(anchor_prefix) {
                return false;
            }
            if let Some(target) = &canonical_target {
                if let Ok(loc_canonical) = std::fs::canonicalize(&loc.file) {
                    if loc_canonical == *target {
                        return true;
                    }
                }
            }
            if loc.file.ends_with(resolved_path) {
                return true;
            }
            if let Some(filename) = &resolved_filename {
                if loc
                    .file
                    .file_name()
                    .is_some_and(|f| f.to_string_lossy() == *filename)
                {
                    return true;
                }
            }
            false
        })
        .map(|(id, loc)| anchor_completion_item(id, loc, closing, trailing))
        .collect();

    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// Resolve xref path completions: `.adoc` files relative to `doc_path`'s parent.
/// Reuses `resolve_path_completions` (which appends `[]` to file entries).
pub(crate) fn resolve_xref_path_completions(
    prefix: &str,
    doc_path: &Path,
    ws: Option<&Workspace>,
    closing: &str,
) -> Vec<CompletionItem> {
    let base = doc_path.parent().unwrap_or(doc_path).to_path_buf();
    // For fresh edits (`closing == "[]"`): place cursor BEFORE `[` (so user can
    // type `#anchor` to chain cross-file anchor completion) with Tab → label slot.
    // When brackets already follow the cursor (`closing == ""`): drop the bracket
    // snippet entirely; cursor lands at the end of the inserted path.
    let file_snippet = if closing == "[]" {
        "${1}[${2:label}]"
    } else {
        ""
    };
    resolve_path_completions(
        prefix,
        doc_path,
        ws,
        &base,
        PathCompletionFilter::Adoc,
        file_snippet,
    )
}

fn anchor_completion_item(
    id: &str,
    loc: &asciidoc_core::types::AnchorLocation,
    closing: &str,
    trailing: Option<TrailingBrackets>,
) -> CompletionItem {
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
    let detail = format!("{filename} ({kind_label})");

    // Three modes:
    // (1) `trailing` is Some → an existing `[…]` follows the cursor. Use a
    //     `TextEdit` whose Range swallows it and emit `id[${1:label}]` so the
    //     `label` placeholder is selected after acceptance.
    // (2) `closing == "[]"` (no trailing) → fresh long-form xref: snippet inserts
    //     `id[${1:label}]` so cursor lands on `label`.
    // (3) Otherwise (e.g. `closing == ">>"` for shorthand or `""` when something
    //     else follows) → plain insert.
    if let Some(t) = trailing {
        let range = Range {
            start: Position {
                line: t.line,
                character: t.cursor_col,
            },
            end: Position {
                line: t.line,
                character: t.end_col,
            },
        };
        let new_text = format!("{id}[${{1:label}}]");
        return CompletionItem {
            label: id.to_string(),
            kind: Some(CompletionItemKind::REFERENCE),
            detail: Some(detail),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit { range, new_text })),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        };
    }

    let (insert_text, format) = if closing == "[]" {
        (
            format!("{id}[${{1:label}}]"),
            Some(InsertTextFormat::SNIPPET),
        )
    } else {
        (format!("{id}{closing}"), None)
    };

    CompletionItem {
        label: id.to_string(),
        kind: Some(CompletionItemKind::REFERENCE),
        detail: Some(detail),
        insert_text: Some(insert_text),
        insert_text_format: format,
        ..Default::default()
    }
}

/// File extension filter for path completions.
#[derive(Clone, Copy)]
enum PathCompletionFilter {
    /// Only `.adoc` files
    Adoc,
    /// Only image files (png, jpg, svg, ...)
    Image,
}

/// Image file extensions for completion filtering.
const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "svg", "webp", "bmp", "tiff", "tif", "ico",
];

/// Generic path completion resolver for `include::`, `image::`, and `xref:` macros.
///
/// Differences are parameterized:
/// - `base_dir`: starting directory for filesystem listing
/// - `ext_filter`: which file extensions to include
/// - `file_snippet`: text appended after the file name when accepted, in LSP
///   snippet syntax (e.g. `"[${1:opts}]"` for include, `"${1}[${2:label}]"` for
///   xref so the cursor lands before `[` and Tab jumps to the label slot).
fn resolve_path_completions(
    prefix: &str,
    doc_path: &Path,
    ws: Option<&Workspace>,
    base_dir: &Path,
    ext_filter: PathCompletionFilter,
    file_snippet: &str,
) -> Vec<CompletionItem> {
    // Attribute substitution: resolve {attr} references in prefix
    let resolved_prefix = match ws {
        Some(ws) => ws.attributes().substitute(doc_path, prefix),
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

    let list_dir = base_dir.join(resolved_dir);

    let Ok(entries) = std::fs::read_dir(&list_dir) else {
        return Vec::new();
    };

    let mut items: Vec<CompletionItem> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();

            if path.is_dir() {
                let dir_name = format!("{name}/");
                if !dir_name.starts_with(filter) && !name.starts_with(filter) {
                    return None;
                }
                let full_path = format!("{original_dir_prefix}{dir_name}");
                Some(CompletionItem {
                    label: dir_name.clone(),
                    kind: Some(CompletionItemKind::FOLDER),
                    insert_text: Some(full_path.clone()),
                    // filter_text must include the typed dir prefix so client-side
                    // fuzzy matchers (e.g. blink.cmp) don't drop the item when the
                    // user has typed `chapters/` and the bare label `intro/` no
                    // longer matches their typed text.
                    filter_text: Some(full_path),
                    ..Default::default()
                })
            } else {
                let matches_filter = match ext_filter {
                    PathCompletionFilter::Adoc => Path::new(&name)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("adoc")),
                    PathCompletionFilter::Image => {
                        Path::new(&name).extension().is_some_and(|ext| {
                            IMAGE_EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e))
                        })
                    }
                };
                if !matches_filter || !name.starts_with(filter) {
                    return None;
                }
                let full_path = format!("{original_dir_prefix}{name}");
                let snippet = format!("{full_path}{file_snippet}");
                Some(CompletionItem {
                    label: name.clone(),
                    kind: Some(CompletionItemKind::FILE),
                    insert_text: Some(snippet),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    // See note above: filter_text must include the typed dir prefix.
                    filter_text: Some(full_path),
                    ..Default::default()
                })
            }
        })
        .collect();

    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// Resolve include-path completions by listing `.adoc` files and subdirectories
/// relative to the document's parent directory, filtered by prefix.
pub(crate) fn resolve_include_completions(
    prefix: &str,
    doc_path: &Path,
    ws: Option<&Workspace>,
) -> Vec<CompletionItem> {
    let base = doc_path.parent().unwrap_or(doc_path).to_path_buf();
    // Cursor lands inside `[]` on the `opts` placeholder for include options.
    resolve_path_completions(
        prefix,
        doc_path,
        ws,
        &base,
        PathCompletionFilter::Adoc,
        "[${1:opts}]",
    )
}

/// Resolve image path completions by listing image files from the document's
/// `imagesdir` directory, filtered by the typed prefix.
pub(crate) fn resolve_image_completions(
    prefix: &str,
    doc_path: &Path,
    ws: Option<&Workspace>,
) -> Vec<CompletionItem> {
    let (base, _) = super::navigation::resolve_image_base_dir(doc_path, ws);
    // Cursor lands inside `[]` on the `alt` placeholder for image alt text.
    resolve_path_completions(
        prefix,
        doc_path,
        ws,
        &base,
        PathCompletionFilter::Image,
        "[${1:alt}]",
    )
}

/// Resolve attribute completions by listing all attributes from the workspace's
/// `AttributeRegistry`, filtered by prefix.
///
/// Returns `CompletionItem`s with:
/// - `label`: attribute name
/// - `kind`: Variable
/// - `detail`: resolved attribute value
/// - `insert_text`: `attr_name}` (appends closing `}`)
pub(crate) fn resolve_attribute_completions(
    prefix: &str,
    doc_path: &Path,
    ws: Option<&Workspace>,
) -> Vec<CompletionItem> {
    let Some(ws) = ws else {
        return Vec::new();
    };

    let mut items: Vec<CompletionItem> = ws
        .attributes()
        .all_attributes(doc_path)
        .into_iter()
        .filter(|(name, _)| name.starts_with(prefix))
        .map(|(name, value)| CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::VARIABLE),
            detail: Some(value.to_string()),
            insert_text: Some(format!("{name}}}")),
            ..Default::default()
        })
        .collect();

    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// Resolve block macro completions filtered by prefix.
fn resolve_block_macro_completions(prefix: &str) -> Vec<CompletionItem> {
    resolve_syntax_catalog(BLOCK_MACROS, prefix, CompletionItemKind::KEYWORD)
}

/// Resolve admonition completions filtered by prefix.
fn resolve_admonition_completions(prefix: &str) -> Vec<CompletionItem> {
    resolve_syntax_catalog(ADMONITIONS, prefix, CompletionItemKind::KEYWORD)
}

/// Resolve document attribute completions filtered by prefix.
fn resolve_doc_attribute_completions(prefix: &str) -> Vec<CompletionItem> {
    resolve_syntax_catalog(DOC_ATTRIBUTES, prefix, CompletionItemKind::PROPERTY)
}

/// Resolve inline macro completions for a known macro name.
fn resolve_inline_macro_completions(macro_name: &str) -> Vec<CompletionItem> {
    INLINE_MACROS
        .iter()
        .filter(|e| e.name == macro_name)
        .map(|e| CompletionItem {
            label: format!("{}:", e.name),
            kind: Some(CompletionItemKind::KEYWORD),
            detail: Some(e.detail.to_string()),
            insert_text: Some(e.snippet.to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        })
        .collect()
}

/// Resolve inline macro prefix completions (no `:` typed yet).
/// Returns items with a `TextEdit` that replaces the typed prefix with the full macro + snippet.
fn resolve_inline_macro_prefix_completions(
    prefix: &str,
    line: u32,
    word_start_col: u32,
    cursor_col: u32,
) -> Vec<CompletionItem> {
    let replace_range = Range {
        start: Position {
            line,
            character: word_start_col,
        },
        end: Position {
            line,
            character: cursor_col,
        },
    };

    INLINE_MACROS
        .iter()
        .filter(|e| e.name.starts_with(prefix))
        .map(|e| {
            let full_snippet = format!("{}:{}", e.name, e.snippet);
            CompletionItem {
                label: format!("{}:", e.name),
                kind: Some(CompletionItemKind::KEYWORD),
                detail: Some(e.detail.to_string()),
                filter_text: Some(e.name.to_string()),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: replace_range,
                    new_text: full_snippet,
                })),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                sort_text: Some(format!("8{}", e.name)),
                ..Default::default()
            }
        })
        .collect()
}

/// Generic resolver: filter a syntax catalog by prefix and build `CompletionItems`.
fn resolve_syntax_catalog(
    catalog: &[SyntaxEntry],
    prefix: &str,
    kind: CompletionItemKind,
) -> Vec<CompletionItem> {
    catalog
        .iter()
        .filter(|e| e.name.starts_with(prefix))
        .map(|e| CompletionItem {
            label: e.name.to_string(),
            kind: Some(kind),
            detail: Some(e.detail.to_string()),
            insert_text: Some(e.snippet.to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        })
        .collect()
}

/// Dispatch completion resolution based on the detected context.
///
/// Routes to the appropriate resolve function:
/// - `Xref` → `resolve_xref_completions`
/// - `IncludePath` → `resolve_include_completions` (uses `doc_path` for relative listing)
/// - `Attribute` → `resolve_attribute_completions`
pub(crate) fn resolve_completions(
    ctx: CompletionContext,
    doc_path: &Path,
    ws: Option<&Workspace>,
) -> Vec<CompletionItem> {
    match ctx {
        CompletionContext::XrefAnchor {
            prefix,
            closing,
            trailing,
        } => resolve_xref_completions(&prefix, ws, closing, trailing),
        CompletionContext::XrefAnchorOrPath {
            prefix,
            closing,
            trailing,
        } => {
            // Discovery-friendly: merge anchor + path completions for `xref:foo`.
            // `closing` is `""` when brackets already exist after the cursor —
            // both arms must respect it to avoid doubling. When `trailing` is
            // Some, anchor items use TextEdit so the existing label is selected.
            let mut items = resolve_xref_completions(&prefix, ws, closing, trailing);
            items.extend(resolve_xref_path_completions(
                &prefix, doc_path, ws, closing,
            ));
            items.sort_by(|a, b| a.label.cmp(&b.label));
            items
        }
        CompletionContext::XrefCrossAnchor {
            file,
            anchor_prefix,
            closing,
            trailing,
        } => resolve_xref_cross_anchor_completions(
            &file,
            &anchor_prefix,
            doc_path,
            ws,
            closing,
            trailing,
        ),
        CompletionContext::IncludePath { prefix } => {
            resolve_include_completions(&prefix, doc_path, ws)
        }
        CompletionContext::ImagePath { prefix } => resolve_image_completions(&prefix, doc_path, ws),
        CompletionContext::Attribute { prefix } => {
            resolve_attribute_completions(&prefix, doc_path, ws)
        }
        CompletionContext::BlockMacro { prefix } => resolve_block_macro_completions(&prefix),
        CompletionContext::Admonition { prefix } => resolve_admonition_completions(&prefix),
        CompletionContext::DocumentAttribute { prefix } => {
            resolve_doc_attribute_completions(&prefix)
        }
        CompletionContext::InlineMacroPrefix {
            prefix,
            line,
            word_start_col,
            cursor_col,
        } => resolve_inline_macro_prefix_completions(&prefix, line, word_start_col, cursor_col),
        CompletionContext::InlineMacro { macro_name } => {
            resolve_inline_macro_completions(&macro_name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("xref shorthand", "See <<my-pre", 0, 12,
        CompletionContext::XrefAnchor { prefix: "my-pre".into(), closing: ">>", trailing: None })]
    #[case("include path", "include::chap", 0, 13,
        CompletionContext::IncludePath { prefix: "chap".into() })]
    #[case("attribute", "The {img", 0, 8,
        CompletionContext::Attribute { prefix: "img".into() })]
    #[case("xref empty prefix", "See <<", 0, 6,
        CompletionContext::XrefAnchor { prefix: "".into(), closing: ">>", trailing: None })]
    #[case("xref multiline", "= Title\n\nSee <<my-id", 2, 11,
        CompletionContext::XrefAnchor { prefix: "my-id".into(), closing: ">>", trailing: None })]
    #[case("xref with umlaut", "Über <<anch", 0, 11,
        CompletionContext::XrefAnchor { prefix: "anch".into(), closing: ">>", trailing: None })]
    #[case("xref shorthand cross-file", "See <<other.adoc#sec", 0, 20,
        CompletionContext::XrefCrossAnchor { file: "other.adoc".into(), anchor_prefix: "sec".into(), closing: ">>", trailing: None })]
    #[case("attribute with CJK", "中{attr", 0, 6,
        CompletionContext::Attribute { prefix: "attr".into() })]
    #[case("inline macro prefix after multibyte", ":imagesdir: Ükbd", 0, 16,
        CompletionContext::InlineMacroPrefix { prefix: "kbd".into(), line: 0, word_start_col: 13, cursor_col: 16 })]
    #[case("inline macro after multibyte", "Ü kbd:", 0, 6,
        CompletionContext::InlineMacro { macro_name: "kbd".into() })]
    #[case("include priority over attribute", "include::{docdir}/chap", 0, 22,
        CompletionContext::IncludePath { prefix: "{docdir}/chap".into() })]
    #[case("include open brace triggers attribute", "include::{snip", 0, 14,
        CompletionContext::Attribute { prefix: "snip".into() })]
    #[case("include closed brace stays include", "include::{snippetsdir}/file", 0, 27,
        CompletionContext::IncludePath { prefix: "{snippetsdir}/file".into() })]
    #[case("xref long form", "See xref:sec", 0, 12,
        CompletionContext::XrefAnchorOrPath { prefix: "sec".into(), closing: "[]", trailing: None })]
    #[case("xref long form open attribute", "See xref:{attr", 0, 14,
        CompletionContext::Attribute { prefix: "attr".into() })]
    #[case("xref long form closed attribute", "See xref:{resolved}/sec", 0, 23,
        CompletionContext::XrefAnchorOrPath { prefix: "{resolved}/sec".into(), closing: "[]", trailing: None })]
    #[case("xref long form cross-file", "See xref:other.adoc#sec", 0, 23,
        CompletionContext::XrefCrossAnchor { file: "other.adoc".into(), anchor_prefix: "sec".into(), closing: "[]", trailing: None })]
    #[case("xref long form same-file via hash", "See xref:#sec", 0, 13,
        CompletionContext::XrefCrossAnchor { file: "".into(), anchor_prefix: "sec".into(), closing: "[]", trailing: None })]
    #[case("xref long form path subdir", "xref:chapters/", 0, 14,
        CompletionContext::XrefAnchorOrPath { prefix: "chapters/".into(), closing: "[]", trailing: None })]
    fn test_detect_completion_some(
        #[case] _name: &str,
        #[case] content: &str,
        #[case] line: u32,
        #[case] character: u32,
        #[case] expected: CompletionContext,
    ) {
        let pos = Position { line, character };
        assert_eq!(detect_completion_context(content, pos), Some(expected));
    }

    #[rstest]
    #[case("plain text", "Just normal text here.", 0, 10)]
    #[case("closed attribute", "The {imagesdir} is set.", 0, 15)]
    #[case("xref long form closed", "See xref:target[Link Text", 0, 25)]
    fn test_detect_completion_none(
        #[case] _name: &str,
        #[case] content: &str,
        #[case] line: u32,
        #[case] character: u32,
    ) {
        let pos = Position { line, character };
        assert_eq!(detect_completion_context(content, pos), None);
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
        Workspace::new_with(
            PathBuf::from("/tmp"),
            AttributeRegistry::new(Path::new("/tmp")),
            IncludeGraph::new(),
            index,
            HashMap::new(),
            HashMap::new(),
        )
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

        let items = resolve_xref_completions("my", Some(&ws), ">>", None);
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

        let items = resolve_xref_completions("", Some(&ws), ">>", None);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "alpha");
        assert_eq!(items[1].label, "beta");
    }

    #[test]
    fn test_xref_completions_no_workspace() {
        let items = resolve_xref_completions("my", None, ">>", None);
        assert!(items.is_empty());
    }

    #[test]
    fn test_xref_completions_xref_form_uses_brackets() {
        let ws = ws_with_anchors(vec![(
            "sec",
            AnchorLocation {
                file: PathBuf::from("/a.adoc"),
                line: 1,
                kind: AnchorKind::Explicit,
            },
        )]);
        let items = resolve_xref_completions("", Some(&ws), "[]", None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].insert_text.as_ref().unwrap(), "sec[${1:label}]");
        assert_eq!(items[0].insert_text_format, Some(InsertTextFormat::SNIPPET));
    }

    // --- Phase 2b: resolve_xref_cross_anchor_completions ---

    #[test]
    fn test_cross_anchor_filters_by_file() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a.adoc");
        let b = dir.path().join("b.adoc");
        std::fs::write(&a, "= A\n").unwrap();
        std::fs::write(&b, "= B\n").unwrap();
        let ws = ws_with_anchors(vec![
            (
                "sec1",
                AnchorLocation {
                    file: a.clone(),
                    line: 1,
                    kind: AnchorKind::ImplicitSection,
                },
            ),
            (
                "sec2",
                AnchorLocation {
                    file: b.clone(),
                    line: 1,
                    kind: AnchorKind::ImplicitSection,
                },
            ),
        ]);

        let doc_path = a.clone();
        let items =
            resolve_xref_cross_anchor_completions("b.adoc", "", &doc_path, Some(&ws), "[]", None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "sec2");
        assert_eq!(items[0].insert_text.as_ref().unwrap(), "sec2[${1:label}]");
    }

    #[test]
    fn test_cross_anchor_empty_file_means_current_doc() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a.adoc");
        let b = dir.path().join("b.adoc");
        std::fs::write(&a, "= A\n").unwrap();
        std::fs::write(&b, "= B\n").unwrap();
        let ws = ws_with_anchors(vec![
            (
                "here",
                AnchorLocation {
                    file: a.clone(),
                    line: 1,
                    kind: AnchorKind::Explicit,
                },
            ),
            (
                "there",
                AnchorLocation {
                    file: b.clone(),
                    line: 1,
                    kind: AnchorKind::Explicit,
                },
            ),
        ]);

        let items = resolve_xref_cross_anchor_completions("", "", &a, Some(&ws), ">>", None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "here");
        assert_eq!(items[0].insert_text.as_ref().unwrap(), "here>>");
    }

    // --- Phase 2c: resolve_xref_path_completions ---

    #[test]
    fn test_xref_path_lists_adoc_siblings() {
        let dir = setup_include_dir();
        let doc = dir.path().join("a.adoc");
        let items = resolve_xref_path_completions("", &doc, None, "[]");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"a.adoc"));
        assert!(labels.contains(&"b.adoc"));
        assert!(labels.contains(&"chapters/"));
        // Files get xref-snippet suffix: cursor before `[`, Tab → label slot
        let b_item = items.iter().find(|i| i.label == "b.adoc").unwrap();
        assert_eq!(
            b_item.insert_text.as_ref().unwrap(),
            "b.adoc${1}[${2:label}]"
        );
        assert_eq!(b_item.insert_text_format, Some(InsertTextFormat::SNIPPET));
    }

    #[test]
    fn test_xref_path_lists_files_in_subdirectory() {
        let dir = setup_include_dir();
        let doc = dir.path().join("a.adoc");
        let items = resolve_xref_path_completions("chapters/", &doc, None, "[]");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["intro.adoc"]);
        let intro = &items[0];
        assert_eq!(
            intro.insert_text.as_deref(),
            Some("chapters/intro.adoc${1}[${2:label}]")
        );
        assert_eq!(intro.filter_text.as_deref(), Some("chapters/intro.adoc"));
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
        // insert_text for files uses include-snippet (cursor inside `[opts]`)
        assert_eq!(a_item.insert_text.as_ref().unwrap(), "a.adoc[${1:opts}]");
        assert_eq!(a_item.insert_text_format, Some(InsertTextFormat::SNIPPET));
        // Directories have Folder kind
        let ch_item = items.iter().find(|i| i.label == "chapters/").unwrap();
        assert_eq!(ch_item.kind, Some(CompletionItemKind::FOLDER));
        // insert_text for dirs is the path so far (no suffix snippet)
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
            "chapters/intro.adoc[${1:opts}]"
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
        attrs.ensure_config_for_dir(dir.path());

        let ws = Workspace::new_with(
            dir.path().to_path_buf(),
            attrs,
            IncludeGraph::new(),
            ReferenceIndex::new(),
            HashMap::new(),
            HashMap::new(),
        );
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
            "{source-dir}/module-a.adoc[${1:opts}]"
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
            "{source-dir}/module-a.adoc[${1:opts}]"
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
    /// Returns (tempdir, workspace, file_path) — file_path is under the tempdir.
    fn ws_with_config_and_file_attrs(
        config_content: &str,
        file_attrs: Vec<AttributeDef>,
    ) -> (TempDir, Workspace, PathBuf) {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join(".asciidoctorconfig"), config_content).unwrap();

        let file = root.join("doc.adoc");
        let mut attrs = AttributeRegistry::new(&root);
        attrs.ensure_config_for_dir(&root);
        attrs.register_file_attributes(&file, &file_attrs);

        let ws = Workspace::new_with(
            root.clone(),
            attrs,
            IncludeGraph::new(),
            ReferenceIndex::new(),
            HashMap::new(),
            HashMap::new(),
        );
        (dir, ws, file)
    }

    #[test]
    fn test_attribute_completions_basic() {
        let (_dir, ws, file) = ws_with_config_and_file_attrs(
            ":imagesdir: images\n",
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
        let (_dir, ws, file) = ws_with_config_and_file_attrs(
            ":imagesdir: images\n",
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
        let ctx = CompletionContext::XrefAnchor {
            prefix: "my".into(),
            closing: ">>",
            trailing: None,
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
        let (_dir, ws, file) = ws_with_config_and_file_attrs(
            ":imagesdir: images\n",
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

    // --- Syntax Completions: BlockMacro detection ---

    #[test]
    fn test_detect_block_macro_at_line_start() {
        let content = "ima";
        let pos = Position {
            line: 0,
            character: 3,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::BlockMacro {
                prefix: "ima".into()
            })
        );
    }

    #[test]
    fn test_detect_block_macro_empty_prefix() {
        // Empty line with cursor at position 0 — no completion (nothing typed)
        let content = "";
        let pos = Position {
            line: 0,
            character: 0,
        };
        assert_eq!(detect_completion_context(content, pos), None);
    }

    #[test]
    fn test_detect_block_macro_not_at_line_start() {
        // "ima" mid-line matches InlineMacroPrefix (image starts with ima)
        let content = "text ima";
        let pos = Position {
            line: 0,
            character: 8,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::InlineMacroPrefix {
                prefix: "ima".into(),
                line: 0,
                word_start_col: 5,
                cursor_col: 8,
            })
        );
    }

    #[test]
    fn test_detect_block_macro_with_uppercase() {
        let content = "Ima";
        let pos = Position {
            line: 0,
            character: 3,
        };
        // Mixed-case is not a block macro (uppercase → Admonition check)
        assert_ne!(
            detect_completion_context(content, pos),
            Some(CompletionContext::BlockMacro {
                prefix: "Ima".into()
            })
        );
    }

    // --- Syntax Completions: BlockMacro resolver ---

    #[test]
    fn test_resolve_block_macro_prefix_filter() {
        let items = resolve_block_macro_completions("im");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "image::");
        assert_eq!(
            items[0].insert_text.as_ref().unwrap(),
            "image::${1:path}[${2:alt}]"
        );
        assert_eq!(items[0].insert_text_format, Some(InsertTextFormat::SNIPPET));
    }

    #[test]
    fn test_resolve_block_macro_no_match() {
        let items = resolve_block_macro_completions("xyz");
        assert!(items.is_empty());
    }

    #[test]
    fn test_resolve_block_macro_all() {
        let items = resolve_block_macro_completions("");
        assert_eq!(items.len(), 9);
    }

    // --- Syntax Completions: Admonition detection ---

    #[test]
    fn test_detect_admonition_at_line_start() {
        let content = "NO";
        let pos = Position {
            line: 0,
            character: 2,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::Admonition {
                prefix: "NO".into()
            })
        );
    }

    #[test]
    fn test_detect_admonition_with_colon() {
        let content = "NOTE:";
        let pos = Position {
            line: 0,
            character: 5,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::Admonition {
                prefix: "NOTE".into()
            })
        );
    }

    #[test]
    fn test_detect_admonition_not_at_line_start() {
        let content = "text NO";
        let pos = Position {
            line: 0,
            character: 7,
        };
        assert_eq!(detect_completion_context(content, pos), None);
    }

    #[test]
    fn test_detect_admonition_lowercase() {
        // "note" is lowercase but not in BLOCK_MACROS, and not an inline macro prefix → None
        let content = "note";
        let pos = Position {
            line: 0,
            character: 4,
        };
        assert_eq!(detect_completion_context(content, pos), None);
    }

    // --- Syntax Completions: Admonition resolver ---

    #[test]
    fn test_resolve_admonition_filter() {
        let items = resolve_admonition_completions("W");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "WARNING:");
        assert_eq!(items[0].insert_text.as_ref().unwrap(), "WARNING: $0");
    }

    #[test]
    fn test_resolve_admonition_all() {
        let items = resolve_admonition_completions("");
        assert_eq!(items.len(), 5);
    }

    // --- Syntax Completions: DocumentAttribute detection ---

    #[test]
    fn test_detect_doc_attribute_at_line_start() {
        let content = ":to";
        let pos = Position {
            line: 0,
            character: 3,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::DocumentAttribute {
                prefix: "to".into()
            })
        );
    }

    #[test]
    fn test_detect_doc_attribute_empty() {
        let content = ":";
        let pos = Position {
            line: 0,
            character: 1,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::DocumentAttribute { prefix: "".into() })
        );
    }

    #[test]
    fn test_detect_doc_attribute_closed() {
        // `:toc:` — closed (second `:`) → None
        let content = ":toc:";
        let pos = Position {
            line: 0,
            character: 5,
        };
        assert_eq!(detect_completion_context(content, pos), None);
    }

    #[test]
    fn test_detect_doc_attribute_not_at_line_start() {
        // `text :to` — `:` not at start → should fall through to InlineMacro or None
        let content = "text :to";
        let pos = Position {
            line: 0,
            character: 8,
        };
        assert_ne!(
            detect_completion_context(content, pos),
            Some(CompletionContext::DocumentAttribute {
                prefix: "to".into()
            })
        );
    }

    // --- Syntax Completions: DocumentAttribute resolver ---

    #[test]
    fn test_resolve_doc_attribute_filter() {
        let items = resolve_doc_attribute_completions("to");
        assert_eq!(items.len(), 2);
        assert!(items.iter().any(|i| i.label == "toc"));
        assert!(items.iter().any(|i| i.label == "toclevels"));
    }

    #[test]
    fn test_resolve_doc_attribute_boolean() {
        let items = resolve_doc_attribute_completions("numbered");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].insert_text.as_ref().unwrap(), "numbered:");
    }

    #[test]
    fn test_resolve_doc_attribute_enum() {
        let items = resolve_doc_attribute_completions("toc");
        assert_eq!(items.len(), 2);
        let toc_item = items.iter().find(|i| i.label == "toc").unwrap();
        assert!(toc_item
            .insert_text
            .as_ref()
            .unwrap()
            .contains("${1|auto,left,right,preamble|}"));
    }

    // --- Syntax Completions: InlineMacro detection ---

    #[test]
    fn test_detect_inline_macro_kbd() {
        let content = "text kbd:";
        let pos = Position {
            line: 0,
            character: 9,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::InlineMacro {
                macro_name: "kbd".into()
            })
        );
    }

    #[test]
    fn test_detect_inline_macro_not_double_colon() {
        // `image::` → double colon, not InlineMacro
        let content = "text image::";
        let pos = Position {
            line: 0,
            character: 12,
        };
        assert_ne!(
            detect_completion_context(content, pos),
            Some(CompletionContext::InlineMacro {
                macro_name: "image".into()
            })
        );
    }

    #[test]
    fn test_detect_inline_macro_unknown_word() {
        let content = "text foo:";
        let pos = Position {
            line: 0,
            character: 9,
        };
        // "foo" is not in INLINE_MACROS catalog → None
        assert_eq!(detect_completion_context(content, pos), None);
    }

    #[test]
    fn test_detect_inline_macro_xref_handled_by_semantic() {
        // `xref:target` → handled by Priority 2 (Xref), not InlineMacro
        let content = "xref:target";
        let pos = Position {
            line: 0,
            character: 11,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::XrefAnchorOrPath {
                prefix: "target".into(),
                closing: "[]",
                trailing: None
            })
        );
    }

    // --- Syntax Completions: InlineMacro resolver ---

    #[test]
    fn test_resolve_inline_macro_kbd() {
        let items = resolve_inline_macro_completions("kbd");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "kbd:");
        assert_eq!(items[0].insert_text.as_ref().unwrap(), "[${1:keys}]");
        assert_eq!(items[0].insert_text_format, Some(InsertTextFormat::SNIPPET));
    }

    #[test]
    fn test_resolve_inline_macro_menu() {
        let items = resolve_inline_macro_completions("menu");
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].insert_text.as_ref().unwrap(),
            "${1:menu}[${2:items}]"
        );
    }

    // --- Syntax Completions: Dispatcher integration ---

    #[test]
    fn test_dispatch_block_macro() {
        let doc_path = Path::new("/tmp/doc.adoc");
        let ctx = CompletionContext::BlockMacro {
            prefix: "im".into(),
        };
        let items = resolve_completions(ctx, doc_path, None);
        assert!(!items.is_empty());
        assert_eq!(items[0].label, "image::");
    }

    #[test]
    fn test_dispatch_admonition() {
        let doc_path = Path::new("/tmp/doc.adoc");
        let ctx = CompletionContext::Admonition {
            prefix: "NO".into(),
        };
        let items = resolve_completions(ctx, doc_path, None);
        assert!(!items.is_empty());
        assert_eq!(items[0].label, "NOTE:");
    }

    #[test]
    fn test_dispatch_doc_attribute() {
        let doc_path = Path::new("/tmp/doc.adoc");
        let ctx = CompletionContext::DocumentAttribute {
            prefix: "to".into(),
        };
        let items = resolve_completions(ctx, doc_path, None);
        assert!(!items.is_empty());
        assert_eq!(items[0].label, "toc");
    }

    #[test]
    fn test_dispatch_inline_macro() {
        let doc_path = Path::new("/tmp/doc.adoc");
        let ctx = CompletionContext::InlineMacro {
            macro_name: "kbd".into(),
        };
        let items = resolve_completions(ctx, doc_path, None);
        assert!(!items.is_empty());
        assert_eq!(items[0].label, "kbd:");
    }

    // --- Syntax Completions: InlineMacroPrefix detection ---

    #[test]
    fn test_detect_inline_macro_prefix_mid_line() {
        // "text kbd" col 8 → InlineMacroPrefix { prefix: "kbd" }
        let content = "text kbd";
        let pos = Position {
            line: 0,
            character: 8,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::InlineMacroPrefix {
                prefix: "kbd".into(),
                line: 0,
                word_start_col: 5,
                cursor_col: 8,
            })
        );
    }

    #[test]
    fn test_detect_inline_macro_prefix_partial() {
        // "text kb" → InlineMacroPrefix { prefix: "kb" } (partial match for kbd)
        let content = "text kb";
        let pos = Position {
            line: 0,
            character: 7,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::InlineMacroPrefix {
                prefix: "kb".into(),
                line: 0,
                word_start_col: 5,
                cursor_col: 7,
            })
        );
    }

    #[test]
    fn test_detect_inline_macro_prefix_at_line_start() {
        // "kbd" at line start → InlineMacroPrefix (NOT BlockMacro — kbd is not in BLOCK_MACROS)
        let content = "kbd";
        let pos = Position {
            line: 0,
            character: 3,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::InlineMacroPrefix {
                prefix: "kbd".into(),
                line: 0,
                word_start_col: 0,
                cursor_col: 3,
            })
        );
    }

    #[test]
    fn test_detect_image_at_line_start_is_block_macro() {
        // "image" at line start → BlockMacro (image:: IS in BLOCK_MACROS)
        let content = "image";
        let pos = Position {
            line: 0,
            character: 5,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::BlockMacro {
                prefix: "image".into()
            })
        );
    }

    #[test]
    fn test_detect_btn_at_line_start_is_prefix() {
        // "btn" at line start → InlineMacroPrefix (not a block macro)
        let content = "btn";
        let pos = Position {
            line: 0,
            character: 3,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::InlineMacroPrefix {
                prefix: "btn".into(),
                line: 0,
                word_start_col: 0,
                cursor_col: 3,
            })
        );
    }

    #[test]
    fn test_detect_unknown_word_no_completion() {
        // "text xyz" → None (unknown word, no macro starts with "xyz")
        let content = "text xyz";
        let pos = Position {
            line: 0,
            character: 8,
        };
        assert_eq!(detect_completion_context(content, pos), None);
    }

    #[test]
    fn test_detect_no_word_boundary_no_completion() {
        // "textkbd" → None (no word boundary, "textkbd" doesn't match any macro)
        let content = "textkbd";
        let pos = Position {
            line: 0,
            character: 7,
        };
        assert_eq!(detect_completion_context(content, pos), None);
    }

    #[test]
    fn test_detect_inline_macro_colon_still_works() {
        // "text kbd:" → InlineMacro (P9, colon present — existing behavior preserved)
        let content = "text kbd:";
        let pos = Position {
            line: 0,
            character: 9,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::InlineMacro {
                macro_name: "kbd".into()
            })
        );
    }

    // --- P6 regression tests ---

    #[test]
    fn test_detect_inc_at_line_start_is_block_macro() {
        // "inc" at line start → BlockMacro (include:: matches)
        let content = "inc";
        let pos = Position {
            line: 0,
            character: 3,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::BlockMacro {
                prefix: "inc".into()
            })
        );
    }

    #[test]
    fn test_detect_ima_at_line_start_is_block_macro() {
        // "ima" at line start → BlockMacro (image:: matches)
        let content = "ima";
        let pos = Position {
            line: 0,
            character: 3,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::BlockMacro {
                prefix: "ima".into()
            })
        );
    }

    // --- InlineMacroPrefix resolver ---

    #[test]
    fn test_resolve_inline_macro_prefix_kbd() {
        let items = resolve_inline_macro_prefix_completions("kbd", 0, 5, 8);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "kbd:");
        // text_edit replaces prefix with full macro + snippet
        let edit = match items[0].text_edit.as_ref().unwrap() {
            CompletionTextEdit::Edit(e) => e,
            _ => panic!("expected Edit variant"),
        };
        assert_eq!(edit.new_text, "kbd:[${1:keys}]");
        assert_eq!(edit.range.start.character, 5);
        assert_eq!(edit.range.end.character, 8);
        assert_eq!(items[0].filter_text.as_ref().unwrap(), "kbd");
        assert_eq!(items[0].insert_text_format, Some(InsertTextFormat::SNIPPET));
    }

    #[test]
    fn test_resolve_inline_macro_prefix_k() {
        // "k" → 1 item (only kbd starts with k)
        let items = resolve_inline_macro_prefix_completions("k", 0, 0, 1);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "kbd:");
    }

    #[test]
    fn test_dispatch_inline_macro_prefix() {
        let doc_path = Path::new("/tmp/doc.adoc");
        let ctx = CompletionContext::InlineMacroPrefix {
            prefix: "kbd".into(),
            line: 0,
            word_start_col: 5,
            cursor_col: 8,
        };
        let items = resolve_completions(ctx, doc_path, None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "kbd:");
    }

    // --- Syntax Completions: Catalog ---

    #[test]
    fn test_syntax_catalog_no_duplicates() {
        use std::collections::HashSet;

        let check = |entries: &[SyntaxEntry], label: &str| {
            let mut names = HashSet::new();
            for entry in entries {
                assert!(
                    names.insert(entry.name),
                    "duplicate name '{}' in {} catalog",
                    entry.name,
                    label
                );
            }
        };

        check(BLOCK_MACROS, "BLOCK_MACROS");
        check(ADMONITIONS, "ADMONITIONS");
        check(DOC_ATTRIBUTES, "DOC_ATTRIBUTES");
        check(INLINE_MACROS, "INLINE_MACROS");
    }

    // -----------------------------------------------------------------------
    // image:: path detection (Task 1.1)
    // -----------------------------------------------------------------------

    #[test]
    fn test_detect_image_path_basic() {
        let content = "image::foto";
        let pos = Position {
            line: 0,
            character: 11,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::ImagePath {
                prefix: "foto".into()
            })
        );
    }

    #[test]
    fn test_detect_image_path_empty() {
        let content = "image::";
        let pos = Position {
            line: 0,
            character: 7,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::ImagePath { prefix: "".into() })
        );
    }

    #[test]
    fn test_detect_image_path_subdir() {
        let content = "image::sub/foto";
        let pos = Position {
            line: 0,
            character: 15,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::ImagePath {
                prefix: "sub/foto".into()
            })
        );
    }

    // -----------------------------------------------------------------------
    // image:: attribute delegation (Task 1.2)
    // -----------------------------------------------------------------------

    #[test]
    fn test_detect_image_path_attr_ref() {
        // image::{doc — open brace without closing } → attribute completion
        let content = "image::{doc";
        let pos = Position {
            line: 0,
            character: 11,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::Attribute {
                prefix: "doc".into()
            })
        );
    }

    #[test]
    fn test_detect_image_path_resolved_attr() {
        // image::{docdir}/foto — attribute is closed → ImagePath
        let content = "image::{docdir}/foto";
        let pos = Position {
            line: 0,
            character: 20,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::ImagePath {
                prefix: "{docdir}/foto".into()
            })
        );
    }

    #[test]
    fn test_detect_image_path_with_bracket() {
        // Once [ is typed, the macro is closed — no completion
        let content = "image::foto.png[";
        let pos = Position {
            line: 0,
            character: 16,
        };
        assert_eq!(detect_completion_context(content, pos), None);
    }

    // -----------------------------------------------------------------------
    // inline image: detection (Task 3.1)
    // -----------------------------------------------------------------------

    #[test]
    fn test_detect_inline_image_path() {
        // image: (single colon) in running text
        let content = "text image:foto";
        let pos = Position {
            line: 0,
            character: 15,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::ImagePath {
                prefix: "foto".into()
            })
        );
    }

    #[test]
    fn test_detect_inline_image_empty() {
        let content = "text image:";
        let pos = Position {
            line: 0,
            character: 11,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::ImagePath { prefix: "".into() })
        );
    }

    #[test]
    fn test_detect_inline_image_with_bracket() {
        // Once [ is typed, the macro is closed — no completion
        let content = "image:foto.png[";
        let pos = Position {
            line: 0,
            character: 15,
        };
        assert_eq!(detect_completion_context(content, pos), None);
    }

    #[test]
    fn test_detect_inline_image_attr_ref() {
        // image:{doc — open brace without closing } → attribute completion
        let content = "image:{doc";
        let pos = Position {
            line: 0,
            character: 10,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::Attribute {
                prefix: "doc".into()
            })
        );
    }

    #[test]
    fn test_detect_block_image_not_matched_by_inline() {
        // image:: (double colon) must still be matched by Priority 1b, not 1c
        let content = "image::foto";
        let pos = Position {
            line: 0,
            character: 11,
        };
        assert_eq!(
            detect_completion_context(content, pos),
            Some(CompletionContext::ImagePath {
                prefix: "foto".into()
            })
        );
    }

    // --- Image Path Resolution (Task 2.1) ---

    #[test]
    fn test_resolve_image_empty_dir() {
        let dir = TempDir::new().unwrap();
        let doc_file = dir.path().join("doc.adoc");
        let items = resolve_image_completions("", &doc_file, None);
        assert!(items.is_empty(), "empty dir should yield no completions");
    }

    #[test]
    fn test_resolve_image_lists_images() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("foto.png"), b"").unwrap();
        std::fs::write(dir.path().join("diag.svg"), b"").unwrap();
        std::fs::write(dir.path().join("readme.txt"), b"").unwrap();
        let doc_file = dir.path().join("doc.adoc");

        let items = resolve_image_completions("", &doc_file, None);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"foto.png"),
            "missing foto.png in {labels:?}"
        );
        assert!(
            labels.contains(&"diag.svg"),
            "missing diag.svg in {labels:?}"
        );
        assert!(
            !labels.contains(&"readme.txt"),
            "readme.txt should be filtered out"
        );
        // insert_text uses image-snippet (cursor inside `[alt]`)
        let png_item = items.iter().find(|i| i.label == "foto.png").unwrap();
        assert_eq!(png_item.insert_text.as_ref().unwrap(), "foto.png[${1:alt}]");
        assert_eq!(png_item.insert_text_format, Some(InsertTextFormat::SNIPPET));
        assert_eq!(png_item.kind, Some(CompletionItemKind::FILE));
    }

    #[test]
    fn test_resolve_image_with_imagesdir() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join(".asciidoctorconfig"), ":imagesdir: images\n").unwrap();
        let images_dir = root.join("images");
        std::fs::create_dir(&images_dir).unwrap();
        std::fs::write(images_dir.join("logo.png"), b"").unwrap();
        std::fs::write(images_dir.join("banner.jpg"), b"").unwrap();

        let file = root.join("doc.adoc");
        let mut attrs = AttributeRegistry::new(&root);
        attrs.ensure_config_for_dir(&root);
        let ws = Workspace::new_with(
            root.clone(),
            attrs,
            IncludeGraph::new(),
            ReferenceIndex::new(),
            HashMap::new(),
            HashMap::new(),
        );

        let items = resolve_image_completions("", &file, Some(&ws));
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"banner.jpg"),
            "missing banner.jpg in {labels:?}"
        );
        assert!(
            labels.contains(&"logo.png"),
            "missing logo.png in {labels:?}"
        );
    }

    #[test]
    fn test_resolve_image_subdir_prefix() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("icon.gif"), b"").unwrap();
        let doc_file = dir.path().join("doc.adoc");

        let items = resolve_image_completions("sub/", &doc_file, None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "icon.gif");
        assert_eq!(
            items[0].insert_text.as_ref().unwrap(),
            "sub/icon.gif[${1:alt}]"
        );
    }

    #[test]
    fn test_resolve_image_prefix_filter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("foto.png"), b"").unwrap();
        std::fs::write(dir.path().join("diagram.svg"), b"").unwrap();
        let doc_file = dir.path().join("doc.adoc");

        let items = resolve_image_completions("foto", &doc_file, None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "foto.png");
    }

    // --- Image Path Dispatch (Task 2.2) ---

    #[test]
    fn test_resolve_image_no_imagesdir() {
        // ImagePath without workspace → lists relative to document
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("screenshot.png"), b"").unwrap();
        let doc_file = dir.path().join("doc.adoc");

        let ctx = CompletionContext::ImagePath { prefix: "".into() };
        let items = resolve_completions(ctx, &doc_file, None);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"screenshot.png"),
            "missing screenshot.png in {labels:?}"
        );
    }
}
