use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use tower_lsp_server::ls_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, InsertTextFormat, Position, Range,
    TextEdit,
};

use asciidoc_core::anchor_scope::AnchorScope;
use asciidoc_core::paths::canonicalize_if_exists;
use asciidoc_core::references::find_target_file;
use asciidoc_core::types::{AnchorKind, AnchorLocation};
use asciidoc_core::workspace::{AttributeScope, Workspace};

use crate::positions;
use crate::scopes::{
    collapse_if_agree, document_dir, path_scopes, scope_docdir, scope_image_base, scope_label,
};

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

/// Built-in styles of a block attribute line (`[NOTE]`): `(name, detail)`.
/// Source: Asciidoctor 2.0.26 (paragraph, listing, example, quote, open,
/// table, literal, pass and sidebar warn on unknown styles; sections and
/// lists never do) and the Asciidoctor documentation for section and list
/// styles. One entry per name; `detail` names every use. Extension and
/// converter-specific styles are left out.
const BLOCK_STYLES: &[(&str, &str)] = &[
    ("NOTE", "Admonition (paragraph or example block)"),
    ("TIP", "Admonition (paragraph or example block)"),
    ("IMPORTANT", "Admonition (paragraph or example block)"),
    ("WARNING", "Admonition (paragraph or example block)"),
    ("CAUTION", "Admonition (paragraph or example block)"),
    (
        "source",
        "Source code listing (paragraph, listing or open block)",
    ),
    ("listing", "Listing block"),
    ("literal", "Literal block"),
    ("quote", "Quote (paragraph, quote or open block)"),
    ("verse", "Verse (paragraph, quote or open block)"),
    ("example", "Example block"),
    ("sidebar", "Sidebar block"),
    ("open", "Open block"),
    ("pass", "Passthrough block"),
    ("stem", "STEM passthrough block (++++)"),
    ("latexmath", "STEM passthrough block (++++)"),
    ("asciimath", "STEM passthrough block (++++)"),
    ("comment", "Comment block (not rendered)"),
    ("normal", "Normal paragraph"),
    ("abstract", "Abstract (block or section)"),
    (
        "partintro",
        "Part introduction (block or section, book only)",
    ),
    ("appendix", "Appendix section"),
    ("glossary", "Glossary (section or description list)"),
    ("bibliography", "Bibliography (section or unordered list)"),
    ("index", "Index section"),
    ("preface", "Preface section"),
    ("colophon", "Colophon section"),
    ("dedication", "Dedication section"),
    ("acknowledgments", "Acknowledgments section"),
    ("discrete", "Discrete heading (not a section)"),
    ("float", "Discrete heading (not a section)"),
    ("arabic", "Ordered list numbering"),
    ("decimal", "Ordered list numbering"),
    ("loweralpha", "Ordered list numbering"),
    ("upperalpha", "Ordered list numbering"),
    ("lowerroman", "Ordered list numbering"),
    ("upperroman", "Ordered list numbering"),
    ("lowergreek", "Ordered list numbering"),
    ("horizontal", "Horizontal description list"),
    ("qanda", "Question and answer list"),
];

/// Languages offered after `[source,`. Suggestions only: Asciidoctor accepts
/// any language name without a warning, and the list is not checked against
/// a highlighter.
const SOURCE_LANGUAGES: &[&str] = &[
    "asciidoc",
    "bash",
    "c",
    "console",
    "cpp",
    "csharp",
    "css",
    "diff",
    "dockerfile",
    "go",
    "groovy",
    "haskell",
    "html",
    "ini",
    "java",
    "javascript",
    "json",
    "kotlin",
    "lua",
    "markdown",
    "perl",
    "php",
    "plaintext",
    "powershell",
    "python",
    "ruby",
    "rust",
    "scala",
    "shell",
    "sql",
    "swift",
    "text",
    "toml",
    "typescript",
    "xml",
    "yaml",
];

// ---------------------------------------------------------------------------
// Completion Context
// ---------------------------------------------------------------------------

/// An existing `[…]` block immediately after the cursor. When present,
/// anchor resolvers extend their `TextEdit` over the existing brackets and
/// re-emit them as a snippet `[${1:label}]`, so the `label` placeholder is
/// selected after the user accepts the completion.
#[derive(Debug, PartialEq, Clone, Copy)]
pub(crate) struct TrailingBrackets {
    pub end_col: u32, // UTF-16 column AFTER closing `]`
}

/// The target typed so far (path or anchor) on the cursor line: from the
/// column right after the macro marker (`xref:`, `<<`, `#`, `include::`,
/// `image:`) up to the cursor, in UTF-16 columns. `end_col` is where the
/// token ends: the cursor, or further right when the cursor sits inside it
/// (`{pr|o}`, `<<|ref-o>>`).
///
/// Every item of a target context carries a `TextEdit` over this span.
/// Without one, clients guess where the typed word starts by their own
/// keyword rules, which stop at `/`, so `chapters/0` filters out
/// `chapters/01.adoc` and accepting an item doubles the directory.
#[derive(Debug, PartialEq, Clone, Copy)]
pub(crate) struct TargetSpan {
    pub line: u32,
    pub start_col: u32,
    pub cursor_col: u32,
    pub end_col: u32,
}

impl TargetSpan {
    /// The span from its start to the cursor.
    fn range(self) -> Range {
        self.range_to(self.cursor_col)
    }

    /// The span from its start to `end_col` on the same line.
    fn range_to(self, end_col: u32) -> Range {
        Range {
            start: Position {
                line: self.line,
                character: self.start_col,
            },
            end: Position {
                line: self.line,
                character: end_col,
            },
        }
    }
}

/// How an anchor completion edits the line: what follows the anchor
/// (`closing`), the typed target it replaces (`span`) and an existing `[…]`
/// block it takes over (`trailing`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct AnchorEdit<'a> {
    pub closing: &'a str,
    pub span: TargetSpan,
    pub trailing: Option<TrailingBrackets>,
}

/// Classification of a completion trigger context.
#[derive(Debug, PartialEq)]
pub(crate) enum CompletionContext {
    /// `<<prefix` — cross-reference anchor (shorthand). `closing` is the suffix
    /// appended to the edit text (`>>` for `<<…>>`, `[]` for `xref:…[]`).
    XrefAnchor {
        prefix: String,
        closing: &'static str,
        span: TargetSpan,
        trailing: Option<TrailingBrackets>,
    },
    /// `xref:prefix` without `#` — could be either a workspace anchor or an
    /// `.adoc` file path. Resolver dispatches to *both* and merges results to
    /// support discovery. `closing` is `"[]"` for fresh edits, or `""` when the
    /// brackets are already present after the cursor (avoid doubling).
    XrefAnchorOrPath {
        prefix: String,
        closing: &'static str,
        span: TargetSpan,
        trailing: Option<TrailingBrackets>,
    },
    /// `xref:file.adoc#anchor_prefix` or `<<file.adoc#anchor_prefix` —
    /// cross-file anchor. Empty `file` means same-document (e.g. `xref:#sec`).
    /// `span` covers `anchor_prefix` only.
    XrefCrossAnchor {
        file: String,
        anchor_prefix: String,
        closing: &'static str,
        span: TargetSpan,
        trailing: Option<TrailingBrackets>,
    },
    /// `include::prefix` — include path completion
    IncludePath { prefix: String, span: TargetSpan },
    /// `image::prefix` — image path completion
    ImagePath { prefix: String, span: TargetSpan },
    /// `{prefix` — attribute reference; `}` may already follow the cursor.
    /// Also an attribute name in an `ifdef::`/`ifndef::` directive (`closing`
    /// is then `""`).
    /// `span` covers the whole name, including the part after the cursor.
    Attribute {
        prefix: String,
        span: TargetSpan,
        /// `}` to append, or `""` when one already follows the cursor.
        closing: &'static str,
    },
    /// `ima` at line start — block macro completion
    BlockMacro { prefix: String },
    /// `NO` at line start — admonition completion
    Admonition { prefix: String },
    /// `:to` at line start — document attribute completion
    DocumentAttribute { prefix: String },
    /// `:toc: le` — value of a catalog attribute with a choice list. `name` is
    /// the catalog name (lowercase); `span` covers the value token.
    AttributeValue {
        name: &'static str,
        prefix: String,
        span: TargetSpan,
    },
    /// `[NO` — style slot of a block attribute line. `span` covers the style
    /// token typed so far and the rest of it after the cursor.
    BlockStyle { prefix: String, span: TargetSpan },
    /// `[source,ru` — language slot of a `source` block attribute line.
    SourceLanguage { prefix: String, span: TargetSpan },
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

/// `">>"` to append to a shorthand xref, or `""` when a `>>` already follows
/// the cursor. The text may continue on the next lines of the paragraph
/// (`<<id,the` newline `text>>`); a later `<<` or a blank line ends the search.
fn shorthand_closing(content: &str, line: u32, after_cursor: &str) -> &'static str {
    let mut rest = after_cursor.to_string();
    for next in content.lines().skip(line as usize + 1) {
        if next.trim().is_empty() {
            break;
        }
        rest.push('\n');
        rest.push_str(next);
    }
    let own_xref = rest.find("<<").map_or(&rest[..], |i| &rest[..i]);
    if own_xref.contains(">>") {
        ""
    } else {
        ">>"
    }
}

/// A character of an attribute name, as Asciidoctor reads it in references
/// and directives.
fn is_attribute_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '-' || c == '_'
}

/// A character of the name in an attribute entry line (`:name:`, `:name!:`
/// without the `!`). ASCII only, unlike `is_attribute_name_char` for
/// references.
fn is_attribute_entry_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// A character of an attribute entry value as far as completion cares
/// (`highlight.js`).
fn is_attribute_value_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')
}

/// For `:name: value-so-far` at line start: the catalog name of an attribute
/// with a choice list and the byte offset where the value starts. Asciidoctor
/// needs a blank (or tab) after the closing `:` and reads the name
/// case-insensitively; the unset forms `:toc!:` / `:!toc:` fall out of the
/// name class.
fn attribute_value_target(before_cursor: &str) -> Option<(&'static str, usize)> {
    let (name, after) = before_cursor.strip_prefix(':')?.split_once(':')?;
    if name.is_empty()
        || !name.chars().all(is_attribute_entry_name_char)
        || !after.starts_with([' ', '\t'])
    {
        return None;
    }
    let value = after.trim_start_matches([' ', '\t']);
    if !value.chars().all(is_attribute_value_char) {
        return None;
    }
    let entry = DOC_ATTRIBUTES
        .iter()
        .find(|e| e.name.eq_ignore_ascii_case(name))?;
    snippet_choices(entry.snippet).is_some().then_some(())?;
    Some((entry.name, before_cursor.len() - value.len()))
}

/// Detect the completion context at the given cursor position by scanning
/// the current line backwards from the cursor.
///
/// Priority: `include::` (longest prefix) > `xref:` > `<<` > `{` > `ifdef::`
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
    // The rest of the attribute name after the cursor (`{pr|o}`) counts too.
    let after_name = after_cursor.trim_start_matches(is_attribute_name_char);
    let utf16_len = |text: &str| -> u32 {
        // LSP uses u32 for column offsets; real-world lines won't exceed u32::MAX.
        #[allow(clippy::cast_possible_truncation)]
        let len = text.encode_utf16().count() as u32;
        len
    };
    let attr_end_col =
        pos.character + utf16_len(&after_cursor[..after_cursor.len() - after_name.len()]);
    let attr_close = if after_name.starts_with('}') { "" } else { "}" };
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
                    end_col: pos.character + utf16_len,
                }
            })
        });

    // The target typed so far starts at byte `start` of the line.
    let span_from = |start: usize| TargetSpan {
        line: pos.line,
        start_col: utf16_len(&line_str[..start]),
        cursor_col: pos.character,
        end_col: pos.character,
    };
    // The same, for an attribute name: it extends to the end of the name.
    let attr_span_from = |start: usize| TargetSpan {
        end_col: attr_end_col,
        ..span_from(start)
    };

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
                    span: attr_span_from(prefix_start + brace_pos + 1),
                    closing: attr_close,
                });
            }
        }
        return Some(CompletionContext::IncludePath {
            prefix: prefix.to_string(),
            span: span_from(prefix_start),
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
                        span: attr_span_from(prefix_start + brace_pos + 1),
                        closing: attr_close,
                    });
                }
            }
            return Some(CompletionContext::ImagePath {
                prefix: prefix.to_string(),
                span: span_from(prefix_start),
            });
        }
    }

    // Priority 1c: image: (inline image path — single colon)
    // Must come after 1b so that image:: is matched first.
    if let Some(idx) = before_cursor.rfind("image:") {
        let prefix_start = idx + "image:".len();
        let after_match = &before_cursor[prefix_start..];
        // Guard: if the char right after "image:" is ':', this is image:: (Priority 1b)
        if !after_match.starts_with(':') {
            let prefix = after_match;
            if !prefix.contains('[') {
                if let Some(brace_pos) = prefix.rfind('{') {
                    let after_brace = &prefix[brace_pos + 1..];
                    if !after_brace.contains('}') {
                        return Some(CompletionContext::Attribute {
                            prefix: after_brace.to_string(),
                            span: attr_span_from(prefix_start + brace_pos + 1),
                            closing: attr_close,
                        });
                    }
                }
                return Some(CompletionContext::ImagePath {
                    prefix: prefix.to_string(),
                    span: span_from(prefix_start),
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
                        span: attr_span_from(prefix_start + brace_pos + 1),
                        closing: attr_close,
                    });
                }
            }
            // Split at `#` for cross-file anchor form `xref:file.adoc#anchor`.
            if let Some(hash_pos) = prefix.find('#') {
                return Some(CompletionContext::XrefCrossAnchor {
                    file: prefix[..hash_pos].to_string(),
                    anchor_prefix: prefix[hash_pos + 1..].to_string(),
                    closing: xref_long_close,
                    span: span_from(prefix_start + hash_pos + 1),
                    trailing: xref_trailing,
                });
            }
            return Some(CompletionContext::XrefAnchorOrPath {
                prefix: prefix.to_string(),
                closing: xref_long_close,
                span: span_from(prefix_start),
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
            let xref_short_close = shorthand_closing(content, pos.line, after_cursor);
            // The anchor token continues after the cursor up to `>>`, `,` or
            // whitespace (`<<|ref-o>>`); accepting a completion replaces it.
            let token_len = after_cursor
                .find(|c: char| c == '>' || c == '<' || c == ',' || c.is_whitespace())
                .unwrap_or(after_cursor.len());
            let short_span_from = |start: usize| TargetSpan {
                end_col: pos.character + utf16_len(&after_cursor[..token_len]),
                ..span_from(start)
            };
            // Cross-file anchor form `<<file.adoc#anchor>>`
            if let Some(hash_pos) = prefix.find('#') {
                return Some(CompletionContext::XrefCrossAnchor {
                    file: prefix[..hash_pos].to_string(),
                    anchor_prefix: prefix[hash_pos + 1..].to_string(),
                    closing: xref_short_close,
                    span: short_span_from(prefix_start + hash_pos + 1),
                    // Shorthand uses `>>` not `[]`, so trailing-bracket logic
                    // doesn't apply; existing tests assume None.
                    trailing: None,
                });
            }
            return Some(CompletionContext::XrefAnchor {
                prefix: prefix.to_string(),
                closing: xref_short_close,
                span: short_span_from(prefix_start),
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
                span: attr_span_from(prefix_start),
                closing: attr_close,
            });
        }
    }

    // Priority 4b: attribute name in an `ifdef::` / `ifndef::` directive. A
    // directive starts in column 0 without `\` (Asciidoctor preprocessor);
    // names are separated by `,` (any) or `+` (all). Nothing is appended: a
    // `,`, `+` or `[` may follow the name.
    if let Some(names) = before_cursor
        .strip_prefix("ifdef::")
        .or_else(|| before_cursor.strip_prefix("ifndef::"))
    {
        if names
            .chars()
            .all(|c| is_attribute_name_char(c) || c == ',' || c == '+')
        {
            let names_start = before_cursor.len() - names.len();
            let prefix_start = names_start + names.rfind([',', '+']).map_or(0, |i| i + 1);
            return Some(CompletionContext::Attribute {
                prefix: before_cursor[prefix_start..].to_string(),
                span: attr_span_from(prefix_start),
                closing: "",
            });
        }
    }

    // Priority 4c: block attribute line — `[` in column 0 (`[[id]]` is a block
    // anchor). Style slot: `[NO`; language slot of `source`: `[source,ru`. The
    // line is still unfinished while typing (Asciidoctor reads `[NO` without
    // `]` as text), so this is a syntax check of the line itself, not a tree
    // question. Must precede Priority 8 (`[li` is no `link:`).
    if let Some(attrs) = before_cursor
        .strip_prefix('[')
        .filter(|attrs| !attrs.starts_with('['))
    {
        fn is_style_char(c: char) -> bool {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_')
        }
        fn is_lang_char(c: char) -> bool {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+')
        }
        let token_span = |start: usize, is_token_char: fn(char) -> bool| {
            let token_len = after_cursor
                .find(|c: char| !is_token_char(c))
                .unwrap_or(after_cursor.len());
            TargetSpan {
                end_col: pos.character + utf16_len(&after_cursor[..token_len]),
                ..span_from(start)
            }
        };
        if attrs.chars().all(is_style_char) {
            return Some(CompletionContext::BlockStyle {
                prefix: attrs.to_string(),
                span: token_span(1, is_style_char),
            });
        }
        if let Some((first, rest)) = attrs.split_once(',') {
            let style = first.split(['#', '.', '%']).next().unwrap_or(first);
            let lang = rest.trim_start_matches(' ');
            if style == "source" && lang.chars().all(is_lang_char) {
                return Some(CompletionContext::SourceLanguage {
                    prefix: lang.to_string(),
                    span: token_span(before_cursor.len() - lang.len(), is_lang_char),
                });
            }
        }
    }

    // Priority 5: DocumentAttribute — `:prefix` at line start, no closing `:`
    if let Some(rest) = before_cursor.strip_prefix(':') {
        if !rest.contains(':') && rest.chars().all(is_attribute_entry_name_char) {
            return Some(CompletionContext::DocumentAttribute {
                prefix: rest.to_string(),
            });
        }
    }

    // Priority 5b: AttributeValue — `:name: val` of an attribute with choices.
    // The value token runs on past the cursor over value characters.
    if let Some((name, value_start)) = attribute_value_target(before_cursor) {
        let token_len = after_cursor
            .find(|c: char| !is_attribute_value_char(c))
            .unwrap_or(after_cursor.len());
        return Some(CompletionContext::AttributeValue {
            name,
            prefix: before_cursor[value_start..].to_string(),
            span: TargetSpan {
                end_col: pos.character + utf16_len(&after_cursor[..token_len]),
                ..span_from(value_start)
            },
        });
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

/// The IDs the top-level documents that read `doc_path` define, with where
/// they are defined. With `files`, the IDs a `file#id` xref to one of those
/// files can name: in a document that includes the file, the IDs the
/// document defines in it (Asciidoctor resolves `file#id` in the whole
/// document, but one defined in another file would jump there); in any
/// other, every ID of the file read as a document of its own, those of its
/// included chapters too, which goto and the diagnostics accept. An ID that
/// several documents define is listed once per definition.
fn anchors_of_documents(
    ws: &Workspace,
    doc_path: &Path,
    files: &[PathBuf],
) -> Vec<(String, AnchorLocation)> {
    let catalogs = ws.anchor_scopes_at(doc_path);
    let ids_of = |catalog: &AnchorScope| -> Vec<(String, AnchorLocation)> {
        catalog
            .ids()
            .map(|(id, location)| (id.to_string(), location.clone()))
            .collect()
    };
    if files.is_empty() {
        return catalogs.iter().flat_map(ids_of).collect();
    }
    let mut anchors = Vec::new();
    for file in files {
        let mut external = false;
        for catalog in &catalogs {
            if catalog.contains_file(file) {
                anchors.extend(ids_of(catalog).into_iter().filter(|(_, location)| {
                    std::fs::canonicalize(&location.file).is_ok_and(|located| located == *file)
                }));
            } else {
                external = true;
            }
        }
        if external {
            anchors.extend(ids_of(&ws.anchor_scope_of(file)));
        }
    }
    anchors
}

/// Where an ID of this kind of definition is listed: explicit IDs first, the
/// IDs generated from a title after them.
fn anchor_rank(kind: AnchorKind) -> u8 {
    match kind {
        AnchorKind::Explicit | AnchorKind::BlockId => 0,
        AnchorKind::ImplicitSection => 1,
    }
}

/// Completion items for `anchors`: explicit IDs first, then by ID, one per
/// ID. Of an ID defined twice the explicit definition is listed, else the
/// first by file and line.
fn anchor_items<'a>(
    anchors: impl IntoIterator<Item = &'a (String, AnchorLocation)>,
    ws: &Workspace,
    edit: AnchorEdit<'_>,
) -> Vec<CompletionItem> {
    let mut anchors: Vec<_> = anchors.into_iter().collect();
    anchors.sort_by(|(a_id, a), (b_id, b)| {
        a_id.cmp(b_id)
            .then_with(|| anchor_rank(a.kind).cmp(&anchor_rank(b.kind)))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });
    anchors.dedup_by(|(a_id, _), (b_id, _)| a_id == b_id);
    anchors.sort_by_key(|(_, location)| anchor_rank(location.kind));
    anchors
        .into_iter()
        .map(|(id, location)| anchor_completion_item(id, location, ws, edit))
        .collect()
}

/// Resolve xref anchor completions by listing the IDs of the top-level
/// documents that read `doc_path`, filtered by prefix. Each item's edit replaces `span`
/// with the anchor, followed by `closing` (`">>"` or `"[]"`). When
/// `trailing` is `Some`, an existing `[…]` is
/// overwritten with a snippet so the `label` placeholder ends up selected.
pub(crate) fn resolve_xref_completions(
    prefix: &str,
    doc_path: &Path,
    ws: Option<&Workspace>,
    closing: &str,
    span: TargetSpan,
    trailing: Option<TrailingBrackets>,
) -> Vec<CompletionItem> {
    let Some(ws) = ws else {
        return Vec::new();
    };

    let edit = AnchorEdit {
        closing,
        span,
        trailing,
    };
    anchor_items(
        anchors_of_documents(ws, doc_path, &[])
            .iter()
            .filter(|(id, _)| id.starts_with(prefix)),
        ws,
        edit,
    )
}

/// Resolve cross-file xref anchor completions: anchors filtered to a specific
/// target file. Empty `file` means the current document.
///
/// The file is matched as go-to-definition matches it:
///   1. Canonical-path equality (handles `../foo.adoc`, symlinks, when target exists)
///   2. Otherwise the indexed file whose path ends in the typed one or has
///      its file name (`references::find_target_file`)
pub(crate) fn resolve_xref_cross_anchor_completions(
    file: &str,
    anchor_prefix: &str,
    doc_path: &Path,
    line: usize,
    ws: Option<&Workspace>,
    edit: AnchorEdit<'_>,
) -> Vec<CompletionItem> {
    let Some(ws) = ws else {
        return Vec::new();
    };

    if file.is_empty() {
        let doc_canonical =
            std::fs::canonicalize(doc_path).unwrap_or_else(|_| doc_path.to_path_buf());
        return anchor_items(
            anchors_of_documents(ws, doc_path, &[])
                .iter()
                .filter(|(id, loc)| {
                    if !id.starts_with(anchor_prefix) {
                        return false;
                    }
                    let loc_canonical =
                        std::fs::canonicalize(&loc.file).unwrap_or_else(|_| loc.file.clone());
                    loc_canonical == doc_canonical
                }),
            ws,
            edit,
        );
    }

    // The file as each scope reads it (ADR-4); anchors of any of them.
    let base = document_dir(doc_path, Some(ws));
    let resolved: Vec<String> = path_scopes(ws, doc_path, line)
        .map(|scope| scope.substitute(file))
        .filter(|resolved| !resolved.contains('{'))
        .collect();
    if resolved.is_empty() {
        return Vec::new();
    }

    // A file that exists is matched by its canonical path (downstream
    // compares against `std::fs::canonicalize(&loc.file)`). One that does not
    // exist at the typed path is the indexed file whose path ends in it or
    // has its name, as go-to-definition and the diagnostics find it.
    let canonical_targets: Vec<PathBuf> = resolved
        .iter()
        .filter_map(|resolved| {
            std::fs::canonicalize(asciidoc_core::paths::join_doc_path(base, resolved))
                .ok()
                .or_else(|| find_target_file(ws.documents(), None, resolved).cloned())
        })
        .collect();
    anchor_items(
        anchors_of_documents(ws, doc_path, &canonical_targets)
            .iter()
            .filter(|(id, _)| id.starts_with(anchor_prefix)),
        ws,
        edit,
    )
}

/// Resolve xref path completions: `.adoc` files relative to each scope's
/// `docdir` (the top-level document's folder, an included file's own folder
/// for its preview), as Asciidoctor resolves xref paths.
/// Reuses `resolve_path_completions` (which appends `[]` to file entries).
pub(crate) fn resolve_xref_path_completions(
    prefix: &str,
    doc_path: &Path,
    line: usize,
    ws: Option<&Workspace>,
    closing: &str,
    span: TargetSpan,
) -> Vec<CompletionItem> {
    let base = document_dir(doc_path, ws);
    let listings = path_listings(prefix, doc_path, line, ws, base.to_path_buf(), |scope| {
        scope_docdir(scope, base)
    });
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
        &listings,
        PathCompletionFilter::Adoc,
        file_snippet,
        span,
    )
}

fn anchor_completion_item(
    id: &str,
    loc: &asciidoc_core::types::AnchorLocation,
    ws: &Workspace,
    edit: AnchorEdit<'_>,
) -> CompletionItem {
    let AnchorEdit {
        closing,
        span,
        trailing,
    } = edit;
    let filename = loc
        .file
        .file_name()
        .map_or_default(|f| f.to_string_lossy().to_string());
    let kind_label = match loc.kind {
        asciidoc_core::types::AnchorKind::Explicit => "explicit",
        asciidoc_core::types::AnchorKind::ImplicitSection => "section",
        asciidoc_core::types::AnchorKind::BlockId => "block",
    };
    let detail = match ws.anchor_title(id, loc) {
        Some(title) => format!("{title} \u{2014} {filename} ({kind_label})"),
        None => format!("{filename} ({kind_label})"),
    };

    // The edit always replaces the typed anchor prefix (`span`). Three modes:
    // (1) `trailing` is Some → an existing `[…]` follows the cursor. The
    //     range also swallows it and the edit emits `id[${1:label}]` so the
    //     `label` placeholder is selected after acceptance.
    // (2) `closing == "[]"` (no trailing) → fresh long-form xref: snippet inserts
    //     `id[${1:label}]` so cursor lands on `label`.
    // (3) Otherwise (e.g. `closing == ">>"` for shorthand or `""` when something
    //     else follows) → plain insert.
    let (range, new_text, format) = match trailing {
        Some(t) => (
            span.range_to(t.end_col),
            format!("{id}[${{1:label}}]"),
            Some(InsertTextFormat::SNIPPET),
        ),
        None if closing == "[]" => (
            span.range(),
            format!("{id}[${{1:label}}]"),
            Some(InsertTextFormat::SNIPPET),
        ),
        None => (span.range_to(span.end_col), format!("{id}{closing}"), None),
    };

    CompletionItem {
        label: id.to_string(),
        kind: Some(CompletionItemKind::REFERENCE),
        detail: Some(detail),
        sort_text: Some(format!("{}{id}", anchor_rank(loc.kind))),
        text_edit: Some(CompletionTextEdit::Edit(TextEdit { range, new_text })),
        insert_text_format: format,
        ..Default::default()
    }
}

/// Which files a path completion offers.
#[derive(Clone, Copy)]
enum PathCompletionFilter<'a> {
    /// Only `.adoc` files
    Adoc,
    /// Every text file except hidden ones and `current`, the canonical path
    /// of the document itself
    IncludeTarget { current: &'a Path },
    /// Only image files (png, jpg, svg, ...)
    Image,
}

/// Image file extensions for completion filtering.
const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "svg", "webp", "bmp", "tiff", "tif", "ico",
];

/// How many leading bytes decide whether a file is text (the value git uses).
const TEXT_SNIFF_LEN: u64 = 8000;

/// Whether `head`, the start of a file, looks like text Asciidoctor could
/// include: no NUL byte and valid UTF-8. A multibyte character cut off at
/// the end of `head` still counts as valid.
fn looks_like_text(head: &[u8]) -> bool {
    if head.contains(&0) {
        return false;
    }
    match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(err) => err.error_len().is_none(),
    }
}

/// Whether the file at `path` is a readable regular file that looks like
/// text. Anything else cannot be included; FIFOs and devices are never
/// opened, since opening a FIFO without a writer blocks.
fn is_text_file(path: &Path) -> bool {
    if !std::fs::metadata(path).is_ok_and(|meta| meta.is_file()) {
        return false;
    }
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = Vec::new();
    if file.take(TEXT_SNIFF_LEN).read_to_end(&mut head).is_err() {
        return false;
    }
    looks_like_text(&head)
}

/// Generic path completion resolver for `include::`, `image::`, and `xref:` macros.
///
/// Differences are parameterized:
/// - `base_dir`: starting directory for filesystem listing
/// - `ext_filter`: which files to offer (see [`PathCompletionFilter`])
/// - `file_snippet`: text appended after the file name when accepted, in LSP
///   snippet syntax (e.g. `"[${1:opts}]"` for include, `"${1}[${2:label}]"` for
///   xref so the cursor lands before `[` and Tab jumps to the label slot).
/// - `span`: the typed `prefix` on the line; each item's edit replaces it
///   with the full path, so clients need not guess where the path starts.
fn resolve_path_completions(
    prefix: &str,
    listings: &[PathListing],
    ext_filter: PathCompletionFilter<'_>,
    file_snippet: &str,
    span: TargetSpan,
) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = Vec::new();
    for listing in listings {
        for item in list_path_items(prefix, listing, ext_filter, file_snippet, span) {
            if !items.iter().any(|known| known.label == item.label) {
                items.push(item);
            }
        }
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// Where one attribute scope lists path candidates: the folder the path
/// starts from and the typed prefix with that scope's values substituted.
struct PathListing {
    base_dir: PathBuf,
    resolved_prefix: String,
}

/// One [`PathListing`] per scope of `doc_path` on `line` (ADR-4), each
/// starting from `base_dir(scope)`; without a workspace one from
/// `fallback_base` with `prefix` as typed.
fn path_listings(
    prefix: &str,
    doc_path: &Path,
    line: usize,
    ws: Option<&Workspace>,
    fallback_base: PathBuf,
    base_dir: impl Fn(&AttributeScope<'_>) -> PathBuf,
) -> Vec<PathListing> {
    let Some(ws) = ws else {
        return vec![PathListing {
            base_dir: fallback_base,
            resolved_prefix: prefix.to_string(),
        }];
    };
    path_scopes(ws, doc_path, line)
        .map(|scope| PathListing {
            base_dir: base_dir(&scope),
            resolved_prefix: scope.substitute(prefix),
        })
        .collect()
}

/// The candidates of one [`PathListing`] for the typed `prefix`.
fn list_path_items(
    prefix: &str,
    listing: &PathListing,
    ext_filter: PathCompletionFilter<'_>,
    file_snippet: &str,
    span: TargetSpan,
) -> Vec<CompletionItem> {
    let resolved_prefix = &listing.resolved_prefix;
    // Bail on unresolved attributes (still contains `{`)
    if resolved_prefix.contains('{') {
        return Vec::new();
    }

    // Split ORIGINAL prefix for the edit text, RESOLVED prefix for filesystem lookup
    let (original_dir_prefix, _) = match prefix.rfind('/') {
        Some(idx) => (&prefix[..=idx], &prefix[idx + 1..]),
        None => ("", prefix),
    };
    let (resolved_dir, filter) = match resolved_prefix.rfind('/') {
        Some(idx) => (&resolved_prefix[..=idx], &resolved_prefix[idx + 1..]),
        None => ("", resolved_prefix.as_str()),
    };

    let list_dir = asciidoc_core::paths::normalize_lexically(&listing.base_dir.join(resolved_dir));

    let Ok(entries) = std::fs::read_dir(&list_dir) else {
        return Vec::new();
    };

    entries
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
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                        range: span.range(),
                        new_text: full_path.clone(),
                    })),
                    // filter_text must include the typed dir prefix so client-side
                    // fuzzy matchers (e.g. blink.cmp) don't drop the item when the
                    // user has typed `chapters/` and the bare label `intro/` no
                    // longer matches their typed text.
                    filter_text: Some(full_path),
                    ..Default::default()
                })
            } else {
                // Cheap checks first; the text sniff reads the file.
                let matches_filter = name.starts_with(filter)
                    && match ext_filter {
                        PathCompletionFilter::Adoc => Path::new(&name)
                            .extension()
                            .is_some_and(|ext| ext.eq_ignore_ascii_case("adoc")),
                        PathCompletionFilter::IncludeTarget { current } => {
                            !name.starts_with('.')
                                && canonicalize_if_exists(path.clone()) != current
                                && is_text_file(&path)
                        }
                        PathCompletionFilter::Image => {
                            Path::new(&name).extension().is_some_and(|ext| {
                                IMAGE_EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e))
                            })
                        }
                    };
                if !matches_filter {
                    return None;
                }
                let full_path = format!("{original_dir_prefix}{name}");
                let snippet = format!("{full_path}{file_snippet}");
                Some(CompletionItem {
                    label: name.clone(),
                    kind: Some(CompletionItemKind::FILE),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                        range: span.range(),
                        new_text: snippet,
                    })),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    // See note above: filter_text must include the typed dir prefix.
                    filter_text: Some(full_path),
                    ..Default::default()
                })
            }
        })
        .collect()
}

/// Resolve include-path completions by listing every text file except hidden
/// ones and the document itself, and subdirectories, relative to the document
/// directory, filtered by prefix.
pub(crate) fn resolve_include_completions(
    prefix: &str,
    doc_path: &Path,
    line: usize,
    ws: Option<&Workspace>,
    span: TargetSpan,
) -> Vec<CompletionItem> {
    let base = document_dir(doc_path, ws);
    let listings = path_listings(prefix, doc_path, line, ws, base.to_path_buf(), |_| {
        base.to_path_buf()
    });
    let current = canonicalize_if_exists(doc_path.to_path_buf());
    // Cursor lands inside `[]` on the `opts` placeholder for include options.
    resolve_path_completions(
        prefix,
        &listings,
        PathCompletionFilter::IncludeTarget { current: &current },
        "[${1:opts}]",
        span,
    )
}

/// Resolve image path completions by listing image files from the document's
/// `imagesdir` directory, filtered by the typed prefix.
pub(crate) fn resolve_image_completions(
    prefix: &str,
    doc_path: &Path,
    line: usize,
    ws: Option<&Workspace>,
    span: TargetSpan,
) -> Vec<CompletionItem> {
    let file_dir = document_dir(doc_path, ws);
    let listings = path_listings(
        prefix,
        doc_path,
        line,
        ws,
        file_dir.to_path_buf(),
        |scope| scope_image_base(scope, file_dir).0,
    );
    // Cursor lands inside `[]` on the `alt` placeholder for image alt text.
    resolve_path_completions(
        prefix,
        &listings,
        PathCompletionFilter::Image,
        "[${1:alt}]",
        span,
    )
}

/// Resolve attribute completions by listing all attributes from the workspace's
/// `AttributeRegistry`, filtered by prefix.
///
/// Returns `CompletionItem`s with:
/// - `label`: attribute name
/// - `kind`: Variable
/// - `detail`: resolved attribute value
/// - `text_edit`: replaces the name typed so far (`span`, including the part
///   after the cursor) with `attr_name` plus `closing` (`}` unless one already
///   follows the cursor)
pub(crate) fn resolve_attribute_completions(
    prefix: &str,
    doc_path: &Path,
    line: usize,
    ws: Option<&Workspace>,
    closing: &str,
    span: TargetSpan,
) -> Vec<CompletionItem> {
    let Some(ws) = ws else {
        return Vec::new();
    };

    // Name → (scope label, value) of each top-level document that sets it.
    let mut values: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for scope in ws.scopes_at(doc_path, line) {
        let label = scope_label(&scope);
        for (name, definition) in scope.known() {
            let Some(value) = definition.value else {
                continue;
            };
            if name.starts_with(prefix) {
                values.entry(name).or_default().push((label.clone(), value));
            }
        }
    }

    values
        .into_iter()
        .map(|(name, mut values)| {
            collapse_if_agree(&mut values, |a, b| a.1 == b.1);
            let detail = if let [(_, value)] = values.as_slice() {
                value.clone()
            } else {
                values
                    .iter()
                    .map(|(label, value)| format!("{label}: {value}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            CompletionItem {
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: span.range_to(span.end_col),
                    new_text: format!("{name}{closing}"),
                })),
                label: name,
                kind: Some(CompletionItemKind::VARIABLE),
                detail: Some(detail),
                ..Default::default()
            }
        })
        .collect()
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

/// Items for `names` that start with `prefix` (ASCII case-insensitive), each
/// replacing the token `span` with the correctly written name.
fn resolve_block_attribute_completions<'a>(
    names: impl Iterator<Item = (&'a str, &'a str)>,
    prefix: &str,
    span: TargetSpan,
    kind: CompletionItemKind,
) -> Vec<CompletionItem> {
    let prefix = prefix.to_ascii_lowercase();
    names
        .filter(|(name, _)| name.to_ascii_lowercase().starts_with(&prefix))
        .map(|(name, detail)| CompletionItem {
            label: name.to_string(),
            kind: Some(kind),
            detail: Some(detail.to_string()),
            filter_text: Some(name.to_string()),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: span.range_to(span.end_col),
                new_text: name.to_string(),
            })),
            ..Default::default()
        })
        .collect()
}

/// Resolve block style completions (`[NO` → `NOTE`).
fn resolve_block_style_completions(prefix: &str, span: TargetSpan) -> Vec<CompletionItem> {
    resolve_block_attribute_completions(
        BLOCK_STYLES.iter().copied(),
        prefix,
        span,
        CompletionItemKind::KEYWORD,
    )
}

/// Resolve source language completions (`[source,ru` → `rust`).
fn resolve_source_language_completions(prefix: &str, span: TargetSpan) -> Vec<CompletionItem> {
    resolve_block_attribute_completions(
        SOURCE_LANGUAGES
            .iter()
            .map(|name| (*name, "Source language")),
        prefix,
        span,
        CompletionItemKind::VALUE,
    )
}

/// The values of a snippet's choice list (`${1|a,b|}`). Placeholders
/// (`${1:images}`) and snippets without a placeholder yield `None`.
fn snippet_choices(snippet: &str) -> Option<impl Iterator<Item = &str>> {
    let (_, rest) = snippet.split_once("${1|")?;
    let (list, _) = rest.split_once("|}")?;
    Some(list.split(','))
}

/// Description and choice values of the catalog document attribute `name`,
/// matched ASCII case-insensitively; no values when its snippet offers no
/// choice list.
pub(crate) fn doc_attribute_doc(name: &str) -> Option<(&'static str, Vec<&'static str>)> {
    let entry = DOC_ATTRIBUTES
        .iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(name))?;
    let values = snippet_choices(entry.snippet).map_or_else(Vec::new, Iterator::collect);
    Some((entry.detail, values))
}

/// Resolve the values of the catalog attribute `name` that start with
/// `prefix`. Each item replaces the value token (`span`).
fn resolve_attribute_value_completions(
    name: &str,
    prefix: &str,
    span: TargetSpan,
) -> Vec<CompletionItem> {
    let Some(entry) = DOC_ATTRIBUTES.iter().find(|e| e.name == name) else {
        return Vec::new();
    };
    snippet_choices(entry.snippet)
        .into_iter()
        .flatten()
        .filter(|value| value.starts_with(prefix))
        .map(|value| CompletionItem {
            label: value.to_string(),
            kind: Some(CompletionItemKind::ENUM_MEMBER),
            detail: Some(entry.detail.to_string()),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: span.range_to(span.end_col),
                new_text: value.to_string(),
            })),
            ..Default::default()
        })
        .collect()
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
///
/// `line` (1-based) is where the completion happens: attribute values are
/// those in effect there (ADR-4).
pub(crate) fn resolve_completions(
    ctx: CompletionContext,
    doc_path: &Path,
    line: usize,
    ws: Option<&Workspace>,
) -> Vec<CompletionItem> {
    match ctx {
        CompletionContext::XrefAnchor {
            prefix,
            closing,
            span,
            trailing,
        } => resolve_xref_completions(&prefix, doc_path, ws, closing, span, trailing),
        CompletionContext::XrefAnchorOrPath {
            prefix,
            closing,
            span,
            trailing,
        } => {
            // Discovery-friendly: merge anchor + path completions for `xref:foo`.
            // `closing` is `""` when brackets already exist after the cursor —
            // both arms must respect it to avoid doubling. When `trailing` is
            // Some, anchor items use TextEdit so the existing label is selected.
            let mut items =
                resolve_xref_completions(&prefix, doc_path, ws, closing, span, trailing);
            items.extend(resolve_xref_path_completions(
                &prefix, doc_path, line, ws, closing, span,
            ));
            // Sorted as a client sorts: by `sortText`, else by label.
            items.sort_by(|a, b| {
                a.sort_text
                    .as_deref()
                    .unwrap_or(&a.label)
                    .cmp(b.sort_text.as_deref().unwrap_or(&b.label))
            });
            items
        }
        CompletionContext::XrefCrossAnchor {
            file,
            anchor_prefix,
            closing,
            span,
            trailing,
        } => resolve_xref_cross_anchor_completions(
            &file,
            &anchor_prefix,
            doc_path,
            line,
            ws,
            AnchorEdit {
                closing,
                span,
                trailing,
            },
        ),
        CompletionContext::IncludePath { prefix, span } => {
            resolve_include_completions(&prefix, doc_path, line, ws, span)
        }
        CompletionContext::ImagePath { prefix, span } => {
            resolve_image_completions(&prefix, doc_path, line, ws, span)
        }
        CompletionContext::Attribute {
            prefix,
            span,
            closing,
        } => resolve_attribute_completions(&prefix, doc_path, line, ws, closing, span),
        CompletionContext::BlockMacro { prefix } => resolve_block_macro_completions(&prefix),
        CompletionContext::Admonition { prefix } => resolve_admonition_completions(&prefix),
        CompletionContext::DocumentAttribute { prefix } => {
            resolve_doc_attribute_completions(&prefix)
        }
        CompletionContext::AttributeValue { name, prefix, span } => {
            resolve_attribute_value_completions(name, &prefix, span)
        }
        CompletionContext::BlockStyle { prefix, span } => {
            resolve_block_style_completions(&prefix, span)
        }
        CompletionContext::SourceLanguage { prefix, span } => {
            resolve_source_language_completions(&prefix, span)
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
    use crate::test_support::scanned;
    use rstest::rstest;

    #[test]
    fn test_snippet_choices_reads_choice_list() {
        let choices: Vec<&str> = snippet_choices("toc: ${1|auto,left,right,preamble|}")
            .unwrap()
            .collect();
        assert_eq!(choices, vec!["auto", "left", "right", "preamble"]);
    }

    #[rstest]
    #[case("imagesdir: ${1:images}")]
    #[case("numbered:")]
    fn test_snippet_choices_ignore_placeholders(#[case] snippet: &str) {
        assert!(snippet_choices(snippet).is_none());
    }

    #[test]
    fn test_doc_attribute_doc_has_description_and_choices() {
        assert_eq!(
            doc_attribute_doc("toc"),
            Some((
                "Table of contents placement",
                vec!["auto", "left", "right", "preamble"]
            ))
        );
    }

    #[test]
    fn test_doc_attribute_doc_without_choices() {
        assert_eq!(
            doc_attribute_doc("imagesdir"),
            Some(("Images directory", vec![]))
        );
        assert_eq!(
            doc_attribute_doc("sectnums"),
            Some(("Number sections", vec![]))
        );
    }

    #[test]
    fn test_doc_attribute_doc_matches_name_case_insensitively() {
        assert_eq!(doc_attribute_doc("TOC"), doc_attribute_doc("toc"));
        assert!(doc_attribute_doc("TOC").is_some());
    }

    #[test]
    fn test_doc_attribute_doc_unknown_name() {
        assert_eq!(doc_attribute_doc("myattr"), None);
        assert_eq!(doc_attribute_doc("pdf-theme"), None);
    }

    /// Span for resolver tests that don't look at where the target starts.
    const AT_LINE_START: TargetSpan = TargetSpan {
        line: 0,
        start_col: 0,
        cursor_col: 0,
        end_col: 0,
    };

    fn span(start_col: u32, cursor_col: u32) -> TargetSpan {
        span_to(start_col, cursor_col, cursor_col)
    }

    /// A span whose token runs on past the cursor, to `end_col`.
    fn span_to(start_col: u32, cursor_col: u32, end_col: u32) -> TargetSpan {
        TargetSpan {
            line: 0,
            start_col,
            cursor_col,
            end_col,
        }
    }

    /// A range on line 0 from `start` to `end` (UTF-16 columns).
    fn cols(start: u32, end: u32) -> Range {
        Range::new(Position::new(0, start), Position::new(0, end))
    }

    /// The edit an item applies on accept; panics if it carries none.
    fn edit(item: &CompletionItem) -> &TextEdit {
        match item.text_edit.as_ref() {
            Some(CompletionTextEdit::Edit(edit)) => edit,
            other => panic!("expected a TextEdit, got {other:?}"),
        }
    }

    fn new_text(item: &CompletionItem) -> &str {
        &edit(item).new_text
    }

    #[rstest]
    #[case("xref shorthand", "See <<my-pre", 0, 12,
        CompletionContext::XrefAnchor { prefix: "my-pre".into(), closing: ">>", span: span(6, 12), trailing: None })]
    #[case("include path", "include::chap", 0, 13,
        CompletionContext::IncludePath { prefix: "chap".into(), span: span(9, 13) })]
    #[case("attribute", "The {img", 0, 8,
        CompletionContext::Attribute { prefix: "img".into(), span: span(5, 8), closing: "}" })]
    #[case("xref empty prefix", "See <<", 0, 6,
        CompletionContext::XrefAnchor { prefix: "".into(), closing: ">>", span: span(6, 6), trailing: None })]
    #[case("xref multiline", "= Title\n\nSee <<my-id", 2, 11,
        CompletionContext::XrefAnchor { prefix: "my-id".into(), closing: ">>", span: TargetSpan { line: 2, start_col: 6, cursor_col: 11, end_col: 11 }, trailing: None })]
    #[case("xref with umlaut", "Über <<anch", 0, 11,
        CompletionContext::XrefAnchor { prefix: "anch".into(), closing: ">>", span: span(7, 11), trailing: None })]
    #[case("xref shorthand cross-file", "See <<other.adoc#sec", 0, 20,
        CompletionContext::XrefCrossAnchor { file: "other.adoc".into(), anchor_prefix: "sec".into(), closing: ">>", span: span(17, 20), trailing: None })]
    #[case("attribute with CJK", "中{attr", 0, 6,
        CompletionContext::Attribute { prefix: "attr".into(), span: span(2, 6), closing: "}" })]
    #[case("inline macro prefix after multibyte", ":imagesdir: Ükbd", 0, 16,
        CompletionContext::InlineMacroPrefix { prefix: "kbd".into(), line: 0, word_start_col: 13, cursor_col: 16 })]
    #[case("inline macro after multibyte", "Ü kbd:", 0, 6,
        CompletionContext::InlineMacro { macro_name: "kbd".into() })]
    #[case("include priority over attribute", "include::{docdir}/chap", 0, 22,
        CompletionContext::IncludePath { prefix: "{docdir}/chap".into(), span: span(9, 22) })]
    #[case("include open brace triggers attribute", "include::{snip", 0, 14,
        CompletionContext::Attribute { prefix: "snip".into(), span: span(10, 14), closing: "}" })]
    #[case("include closed brace stays include", "include::{snippetsdir}/file", 0, 27,
        CompletionContext::IncludePath { prefix: "{snippetsdir}/file".into(), span: span(9, 27) })]
    #[case("xref long form", "See xref:sec", 0, 12,
        CompletionContext::XrefAnchorOrPath { prefix: "sec".into(), closing: "[]", span: span(9, 12), trailing: None })]
    #[case("xref long form open attribute", "See xref:{attr", 0, 14,
        CompletionContext::Attribute { prefix: "attr".into(), span: span(10, 14), closing: "}" })]
    #[case("xref long form closed attribute", "See xref:{resolved}/sec", 0, 23,
        CompletionContext::XrefAnchorOrPath { prefix: "{resolved}/sec".into(), closing: "[]", span: span(9, 23), trailing: None })]
    #[case("xref long form cross-file", "See xref:other.adoc#sec", 0, 23,
        CompletionContext::XrefCrossAnchor { file: "other.adoc".into(), anchor_prefix: "sec".into(), closing: "[]", span: span(20, 23), trailing: None })]
    #[case("xref long form same-file via hash", "See xref:#sec", 0, 13,
        CompletionContext::XrefCrossAnchor { file: "".into(), anchor_prefix: "sec".into(), closing: "[]", span: span(10, 13), trailing: None })]
    #[case("xref long form path subdir", "xref:chapters/", 0, 14,
        CompletionContext::XrefAnchorOrPath { prefix: "chapters/".into(), closing: "[]", span: span(5, 14), trailing: None })]
    #[case("xref path after multibyte counts UTF-16", "Über xref:chapters/", 0, 19,
        CompletionContext::XrefAnchorOrPath { prefix: "chapters/".into(), closing: "[]", span: span(10, 19), trailing: None })]
    #[case("xref before existing brackets", "xref:sec[Text]", 0, 8,
        CompletionContext::XrefAnchorOrPath { prefix: "sec".into(), closing: "", span: span(5, 8), trailing: Some(TrailingBrackets { end_col: 14 }) })]
    #[case("inline image path after multibyte", "Ü image:sub/", 0, 12,
        CompletionContext::ImagePath { prefix: "sub/".into(), span: span(8, 12) })]
    #[case("block image path", "image::sub/", 0, 11,
        CompletionContext::ImagePath { prefix: "sub/".into(), span: span(7, 11) })]
    #[case("attribute before existing closing brace", "Use {pro} now.", 0, 8,
        CompletionContext::Attribute { prefix: "pro".into(), span: span(5, 8), closing: "" })]
    #[case("attribute in include before existing closing brace", "include::{snip}/a.adoc[]", 0, 14,
        CompletionContext::Attribute { prefix: "snip".into(), span: span(10, 14), closing: "" })]
    #[case("attribute mid-name before existing closing brace", "Use {pro} now.", 0, 7,
        CompletionContext::Attribute { prefix: "pr".into(), span: span_to(5, 7, 8), closing: "" })]
    #[case("attribute with later unrelated closing", "Use {pro and {x}", 0, 8,
        CompletionContext::Attribute { prefix: "pro".into(), span: span(5, 8), closing: "}" })]
    #[case("attribute with closing after a space", "Use {pro now}", 0, 8,
        CompletionContext::Attribute { prefix: "pro".into(), span: span(5, 8), closing: "}" })]
    #[case("xref shorthand right after opener before existing target", "<<ref-o>>", 0, 2,
        CompletionContext::XrefAnchor { prefix: "".into(), closing: "", span: span_to(2, 2, 7), trailing: None })]
    #[case("xref shorthand before comma and later closing", "<<setup-install,the install>>", 0, 15,
        CompletionContext::XrefAnchor { prefix: "setup-install".into(), closing: "", span: span(2, 15), trailing: None })]
    #[case("xref shorthand ignores closing of a later xref", "<<a and <<b>>", 0, 3,
        CompletionContext::XrefAnchor { prefix: "a".into(), closing: ">>", span: span(2, 3), trailing: None })]
    #[case("xref shorthand cross-file before comma and later closing", "<<f.adoc#sec,text>>", 0, 12,
        CompletionContext::XrefCrossAnchor { file: "f.adoc".into(), anchor_prefix: "sec".into(), closing: "", span: span(9, 12), trailing: None })]
    #[case("ifdef empty name", "ifdef::", 0, 7,
        CompletionContext::Attribute { prefix: "".into(), span: span(7, 7), closing: "" })]
    #[case("ifdef name prefix", "ifdef::sectn", 0, 12,
        CompletionContext::Attribute { prefix: "sectn".into(), span: span(7, 12), closing: "" })]
    #[case("ifndef name prefix", "ifndef::sec", 0, 11,
        CompletionContext::Attribute { prefix: "sec".into(), span: span(8, 11), closing: "" })]
    #[case("ifdef second name after comma", "ifdef::a,sec", 0, 12,
        CompletionContext::Attribute { prefix: "sec".into(), span: span(9, 12), closing: "" })]
    #[case("ifdef second name after plus", "ifdef::a+sec", 0, 12,
        CompletionContext::Attribute { prefix: "sec".into(), span: span(9, 12), closing: "" })]
    #[case("ifdef mid-name before brackets", "ifdef::sec[]", 0, 9,
        CompletionContext::Attribute { prefix: "se".into(), span: span_to(7, 9, 10), closing: "" })]
    #[case("ifdef name that starts like an inline macro", "ifdef::im", 0, 9,
        CompletionContext::Attribute { prefix: "im".into(), span: span(7, 9), closing: "" })]
    #[case("ifdef name after multibyte counts UTF-16", "ifdef::äb", 0, 9,
        CompletionContext::Attribute { prefix: "äb".into(), span: span(7, 9), closing: "" })]
    #[case("attribute in single-line ifdef content", "ifdef::a[{pro", 0, 13,
        CompletionContext::Attribute { prefix: "pro".into(), span: span(10, 13), closing: "}" })]
    #[case("attribute value empty", ":toc: ", 0, 6,
        CompletionContext::AttributeValue { name: "toc", prefix: "".into(), span: span(6, 6) })]
    #[case("attribute value prefix", ":toc: le", 0, 8,
        CompletionContext::AttributeValue { name: "toc", prefix: "le".into(), span: span(6, 8) })]
    #[case("attribute value after several blanks", ":toc:   le", 0, 10,
        CompletionContext::AttributeValue { name: "toc", prefix: "le".into(), span: span(8, 10) })]
    #[case("attribute value after tab", ":toc:\tle", 0, 8,
        CompletionContext::AttributeValue { name: "toc", prefix: "le".into(), span: span(6, 8) })]
    #[case("attribute value with dot", ":source-highlighter: highlight.", 0, 31,
        CompletionContext::AttributeValue { name: "source-highlighter", prefix: "highlight.".into(), span: span(21, 31) })]
    #[case("attribute value mid-token", ":toc: left", 0, 8,
        CompletionContext::AttributeValue { name: "toc", prefix: "le".into(), span: span_to(6, 8, 10) })]
    #[case("attribute value edit stops at a blank after the cursor", ":toc: left x", 0, 8,
        CompletionContext::AttributeValue { name: "toc", prefix: "le".into(), span: span_to(6, 8, 10) })]
    #[case("attribute value edit stops at a non-value character", ":toc: le{x}", 0, 8,
        CompletionContext::AttributeValue { name: "toc", prefix: "le".into(), span: span_to(6, 8, 8) })]
    #[case("attribute value name case-insensitive", ":TOC: ", 0, 6,
        CompletionContext::AttributeValue { name: "toc", prefix: "".into(), span: span(6, 6) })]
    #[case("attribute value wins over inline macro", ":icons: fo", 0, 10,
        CompletionContext::AttributeValue { name: "icons", prefix: "fo".into(), span: span(8, 10) })]
    #[case("attribute value wins over link macro", ":toc: l", 0, 7,
        CompletionContext::AttributeValue { name: "toc", prefix: "l".into(), span: span(6, 7) })]
    #[case("attribute reference in value stays an attribute", ":toc: {doc", 0, 10,
        CompletionContext::Attribute { prefix: "doc".into(), span: span(7, 10), closing: "}" })]
    #[case("block style prefix", "[NO", 0, 3,
        CompletionContext::BlockStyle { prefix: "NO".into(), span: span(1, 3) })]
    #[case("block style after bare bracket", "[", 0, 1,
        CompletionContext::BlockStyle { prefix: "".into(), span: span(1, 1) })]
    #[case("block style edit stops before the closing bracket", "[NO]", 0, 3,
        CompletionContext::BlockStyle { prefix: "NO".into(), span: span(1, 3) })]
    #[case("block style edit covers the rest of the token only", "[NOTE,x]", 0, 3,
        CompletionContext::BlockStyle { prefix: "NO".into(), span: span_to(1, 3, 5) })]
    #[case("block style wins over inline macro prefix", "[li", 0, 3,
        CompletionContext::BlockStyle { prefix: "li".into(), span: span(1, 3) })]
    #[case("source language prefix", "[source,ru", 0, 10,
        CompletionContext::SourceLanguage { prefix: "ru".into(), span: span(8, 10) })]
    #[case("source language after bare comma", "[source,", 0, 8,
        CompletionContext::SourceLanguage { prefix: "".into(), span: span(8, 8) })]
    #[case("source language ignores blanks after the comma", "[source, ru", 0, 11,
        CompletionContext::SourceLanguage { prefix: "ru".into(), span: span(9, 11) })]
    #[case("source language after a role shorthand", "[source.role,ru", 0, 15,
        CompletionContext::SourceLanguage { prefix: "ru".into(), span: span(13, 15) })]
    #[case("source language after id and option shorthands", "[source#sid%linenums,ru", 0, 23,
        CompletionContext::SourceLanguage { prefix: "ru".into(), span: span(21, 23) })]
    #[case("source language edit stops before the closing bracket", "[source,ru]", 0, 10,
        CompletionContext::SourceLanguage { prefix: "ru".into(), span: span_to(8, 10, 10) })]
    #[case("source language edit covers the rest of the token", "[source,rust]", 0, 10,
        CompletionContext::SourceLanguage { prefix: "ru".into(), span: span_to(8, 10, 12) })]
    #[case("source language may contain a plus", "[source,c+", 0, 10,
        CompletionContext::SourceLanguage { prefix: "c+".into(), span: span(8, 10) })]
    #[case("source language wins over link macro", "[source,li", 0, 10,
        CompletionContext::SourceLanguage { prefix: "li".into(), span: span(8, 10) })]
    #[case("attribute reference in the language slot stays an attribute", "[source,{lan", 0, 12,
        CompletionContext::Attribute { prefix: "lan".into(), span: span(9, 12), closing: "}" })]
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
    #[case(
        "ifdef with leading space is no directive (Asciidoctor: literal text)",
        " ifdef::sec",
        0,
        11
    )]
    #[case(
        "escaped ifdef is no directive (Asciidoctor: text)",
        "\\ifdef::sec",
        0,
        11
    )]
    #[case(
        "ifdef mid-line is no directive (Asciidoctor: text)",
        "text ifdef::sec",
        0,
        15
    )]
    #[case("ifdef after closed brackets", "ifdef::sectnums[]", 0, 17)]
    #[case("ifdef content after bracket", "ifdef::a[sec", 0, 12)]
    #[case("ifdef name list broken by space", "ifdef::a sec", 0, 12)]
    #[case(
        "attribute value needs a blank after the colon (Asciidoctor: no entry)",
        ":toc:left",
        0,
        9
    )]
    #[case("unset suffix form has no value", ":toc!: ", 0, 7)]
    #[case("unset prefix form has no value", ":!toc: ", 0, 7)]
    #[case("attribute without choices", ":imagesdir: ", 0, 12)]
    #[case("attribute not in the catalog", ":custom: ", 0, 9)]
    #[case("second word is no value", ":toc: left x", 0, 12)]
    #[case("attribute entry not at line start", "text :toc: ", 0, 11)]
    #[case("block anchor is no attribute list", "[[NO", 0, 4)]
    #[case("indented bracket is literal text (Asciidoctor)", " [NO", 0, 4)]
    #[case(
        "blank after the bracket is no attribute line (Asciidoctor)",
        "[ NO",
        0,
        4
    )]
    #[case("bracket mid-line", "See [NO", 0, 7)]
    #[case("cursor after the closing bracket", "[NOTE]", 0, 6)]
    #[case("id shorthand has no style slot", "[#id", 0, 4)]
    #[case("role shorthand has no style slot", "[.role", 0, 6)]
    #[case("option shorthand has no style slot", "[%opt", 0, 5)]
    #[case("named attribute has no style slot", "[role=x", 0, 7)]
    #[case("shorthand after the style ends the style slot", "[NOTE#id", 0, 8)]
    #[case("second slot of quote is an attribution", "[quote,Au", 0, 9)]
    #[case("second slot without style depends on the block", "[,ru", 0, 4)]
    #[case("style names are case-sensitive (Asciidoctor)", "[Source,ru", 0, 10)]
    #[case("third slot has no language", "[source,rust,ru", 0, 15)]
    fn test_detect_completion_none(
        #[case] _name: &str,
        #[case] content: &str,
        #[case] line: u32,
        #[case] character: u32,
    ) {
        let pos = Position { line, character };
        assert_eq!(detect_completion_context(content, pos), None);
    }

    // --- resolve_xref_completions ---

    use asciidoc_core::attributes::AttributeRegistry;
    use asciidoc_core::graph::IncludeGraph;
    use asciidoc_core::types::{Anchor, AnchorKind, AnchorLocation, DocumentModel, Section};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    /// Helper: build a minimal Workspace whose documents define `anchors`,
    /// each document a top-level document of its own. An implicit section is
    /// a section titled after its ID, which therefore starts with `_` and
    /// has no other separator than `_`.
    fn ws_with_anchors(anchors: Vec<(&str, AnchorLocation)>) -> Workspace {
        ws_with_anchors_at(Path::new("/tmp"), anchors)
    }

    /// Like [`ws_with_anchors`], with `root` as the workspace root and so as
    /// the `docdir` its documents resolve paths against.
    fn ws_with_anchors_at(root: &Path, anchors: Vec<(&str, AnchorLocation)>) -> Workspace {
        let mut documents: HashMap<PathBuf, DocumentModel> = HashMap::new();
        for (id, loc) in anchors {
            let doc = documents
                .entry(loc.file.clone())
                .or_insert_with(|| DocumentModel {
                    path: loc.file.clone(),
                    sections: vec![],
                    includes: vec![],
                    xrefs: vec![],
                    anchors: vec![],
                    attributes: vec![],
                    attribute_refs: vec![],
                    conditionals: vec![],
                });
            if loc.kind == AnchorKind::ImplicitSection {
                doc.sections.push(Section {
                    level: 1,
                    title: id.trim_start_matches('_').replace('_', " "),
                    line: loc.line,
                    id: None,
                    is_header: false,
                });
            } else {
                doc.anchors.push(Anchor {
                    id: id.to_string(),
                    reftext: None,
                    line: loc.line,
                });
            }
        }
        Workspace::new_with(
            root.to_path_buf(),
            AttributeRegistry::new(root),
            IncludeGraph::new(),
            documents,
            HashMap::new(),
        )
    }

    #[test]
    fn test_xref_completions_basic() {
        let ws = ws_with_anchors(vec![
            (
                "_my-anchor",
                AnchorLocation {
                    file: PathBuf::from("/a.adoc"),
                    line: 5,
                    kind: AnchorKind::Explicit,
                },
            ),
            (
                "_my_section",
                AnchorLocation {
                    file: PathBuf::from("/a.adoc"),
                    line: 10,
                    kind: AnchorKind::ImplicitSection,
                },
            ),
            (
                "_other-id",
                AnchorLocation {
                    file: PathBuf::from("/b.adoc"),
                    line: 1,
                    kind: AnchorKind::Explicit,
                },
            ),
        ]);

        let items = resolve_xref_completions(
            "_my",
            Path::new("/a.adoc"),
            Some(&ws),
            ">>",
            AT_LINE_START,
            None,
        );
        assert_eq!(items.len(), 2);
        // Sorted alphabetically
        assert_eq!(items[0].label, "_my-anchor");
        assert_eq!(items[1].label, "_my_section");
        // Kind
        assert_eq!(items[0].kind, Some(CompletionItemKind::REFERENCE));
        // Detail contains filename + kind
        assert!(items[0].detail.as_ref().unwrap().contains("a.adoc"));
        assert!(items[0].detail.as_ref().unwrap().contains("explicit"));
        assert!(items[1].detail.as_ref().unwrap().contains("section"));
        // edit text appends >>
        assert_eq!(new_text(&items[0]), "_my-anchor>>");
        assert_eq!(new_text(&items[1]), "_my_section>>");
    }

    #[test]
    fn test_xref_completions_empty_prefix() {
        let ws = ws_with_anchors(vec![
            (
                "_alpha",
                AnchorLocation {
                    file: PathBuf::from("/a.adoc"),
                    line: 1,
                    kind: AnchorKind::Explicit,
                },
            ),
            (
                "_beta",
                AnchorLocation {
                    file: PathBuf::from("/a.adoc"),
                    line: 2,
                    kind: AnchorKind::ImplicitSection,
                },
            ),
        ]);

        let items = resolve_xref_completions(
            "",
            Path::new("/a.adoc"),
            Some(&ws),
            ">>",
            AT_LINE_START,
            None,
        );
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "_alpha");
        assert_eq!(items[1].label, "_beta");
    }

    #[test]
    fn test_xref_completions_no_workspace() {
        let items =
            resolve_xref_completions("my", Path::new("/a.adoc"), None, ">>", AT_LINE_START, None);
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
        // `xref:se` with the cursor at the end of the line
        let typed = span(5, 7);
        let items =
            resolve_xref_completions("se", Path::new("/a.adoc"), Some(&ws), "[]", typed, None);
        assert_eq!(items.len(), 1);
        assert_eq!(edit(&items[0]).range, cols(5, 7));
        assert_eq!(new_text(&items[0]), "sec[${1:label}]");
        assert_eq!(items[0].insert_text, None);
        assert_eq!(items[0].insert_text_format, Some(InsertTextFormat::SNIPPET));
    }

    #[test]
    fn test_xref_anchor_edit_swallows_existing_brackets() {
        let ws = ws_with_anchors(vec![(
            "sec",
            AnchorLocation {
                file: PathBuf::from("/a.adoc"),
                line: 1,
                kind: AnchorKind::Explicit,
            },
        )]);
        // `xref:se[Text]` with the cursor before `[`
        let typed = span(5, 7);
        let trailing = TrailingBrackets { end_col: 13 };
        let items = resolve_xref_completions(
            "se",
            Path::new("/a.adoc"),
            Some(&ws),
            "",
            typed,
            Some(trailing),
        );
        assert_eq!(edit(&items[0]).range, cols(5, 13));
        assert_eq!(new_text(&items[0]), "sec[${1:label}]");
    }

    // --- resolve_xref_cross_anchor_completions ---

    #[test]
    fn test_cross_anchor_filters_by_file() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a.adoc");
        let b = dir.path().join("b.adoc");
        std::fs::write(&a, "= A\n").unwrap();
        std::fs::write(&b, "= B\n").unwrap();
        let ws = ws_with_anchors(vec![
            (
                "_sec1",
                AnchorLocation {
                    file: a.clone(),
                    line: 1,
                    kind: AnchorKind::ImplicitSection,
                },
            ),
            (
                "_sec2",
                AnchorLocation {
                    file: b.clone(),
                    line: 1,
                    kind: AnchorKind::ImplicitSection,
                },
            ),
        ]);

        let doc_path = a.clone();
        let items = resolve_xref_cross_anchor_completions(
            "b.adoc",
            "",
            &doc_path,
            1,
            Some(&ws),
            AnchorEdit {
                closing: "[]",
                span: AT_LINE_START,
                trailing: None,
            },
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "_sec2");
        assert_eq!(new_text(&items[0]), "_sec2[${1:label}]");
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

        let items = resolve_xref_cross_anchor_completions(
            "",
            "",
            &a,
            1,
            Some(&ws),
            AnchorEdit {
                closing: ">>",
                span: AT_LINE_START,
                trailing: None,
            },
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "here");
        assert_eq!(new_text(&items[0]), "here>>");
    }

    // --- IDs per top-level document (fixture `anchor_scope`) ---

    fn anchor_scope_fixture() -> (PathBuf, Workspace) {
        fixture("anchor_scope")
    }

    fn fixture(name: &str) -> (PathBuf, Workspace) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(name)
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();
        (root, ws)
    }

    /// The labels completion offers for `typed` at the end of the first line
    /// of `file` in the fixture.
    fn fixture_labels(file: &str, typed: &str) -> Vec<String> {
        labels_in(anchor_scope_fixture(), file, typed)
    }

    fn labels_in((root, ws): (PathBuf, Workspace), file: &str, typed: &str) -> Vec<String> {
        let doc = root.join(file);
        let column = u32::try_from(typed.len()).unwrap();
        let ctx = detect_completion_context(typed, Position::new(0, column)).unwrap();
        resolve_completions(ctx, &doc, 1, Some(&ws))
            .into_iter()
            .map(|item| item.label)
            .collect()
    }

    /// `solo/main.adoc` and `solo2/main.adoc` are documents of their own:
    /// `<<` offers the IDs each defines, whichever of the two asks, and
    /// none of `book.adoc` (a document header gets no ID).
    #[test]
    fn test_xref_completions_offer_only_the_ids_of_the_top_level_document() {
        // Explicit IDs come before generated ones (deliberate, adoc-0210).
        assert_eq!(
            fixture_labels("solo/main.adoc", "<<"),
            vec!["note", "_first", "_second"]
        );
        assert_eq!(
            fixture_labels("solo2/main.adoc", "<<"),
            vec!["note", "_first", "_second"]
        );
        let book = fixture_labels("book.adoc", "<<");
        assert!(book.contains(&"setup-only".to_string()), "got: {book:?}");
        assert!(!book.contains(&"note".to_string()), "got: {book:?}");
    }

    /// The items `<<` offers in `file` of the fixture.
    fn items_in((root, ws): (PathBuf, Workspace), file: &str, typed: &str) -> Vec<CompletionItem> {
        let doc = root.join(file);
        let column = u32::try_from(typed.len()).unwrap();
        let ctx = detect_completion_context(typed, Position::new(0, column)).unwrap();
        resolve_completions(ctx, &doc, 1, Some(&ws))
    }

    /// Asciidoctor links `<<intro>>` as "Introduction": the detail names the
    /// section the ID belongs to. An anchor that is no section title
    /// (`para`) gets no title, not that of the next section.
    #[test]
    fn test_xref_completion_detail_names_the_section_title() {
        let items = items_in(fixture("xref_completion_titles"), "index.adoc", "<<");
        let detail = |label: &str| {
            items
                .iter()
                .find(|item| item.label == label)
                .and_then(|item| item.detail.clone())
        };
        assert_eq!(
            detail("intro").as_deref(),
            Some("Introduction \u{2014} index.adoc (explicit)")
        );
        assert_eq!(
            detail("_audience").as_deref(),
            Some("Audience \u{2014} index.adoc (section)")
        );
        assert_eq!(
            detail("top").as_deref(),
            Some("Product Manual \u{2014} index.adoc (explicit)")
        );
        assert_eq!(detail("para").as_deref(), Some("index.adoc (explicit)"));
    }

    /// `intro.adoc` is included with `leveloffset=+1`; its level-0 title is
    /// the section `[[intro]]` names.
    #[test]
    fn test_xref_completion_detail_titles_an_included_chapter() {
        let items = items_in(fixture("adversarial_project"), "chapters/setup.adoc", "<<");
        let intro = items.iter().find(|item| item.label == "intro").unwrap();
        assert_eq!(
            intro.detail.as_deref(),
            Some("Introduction \u{2014} intro.adoc (explicit)")
        );
    }

    /// Explicit IDs are listed before the IDs Asciidoctor generated from a
    /// title (`_audience`).
    #[test]
    fn test_xref_completions_list_explicit_ids_before_generated_ones() {
        let items = items_in(fixture("xref_completion_titles"), "index.adoc", "<<");
        let position = |wanted: &str| {
            items
                .iter()
                .enumerate()
                .filter(|(_, item)| item.detail.as_deref().is_some_and(|d| d.ends_with(wanted)))
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        };
        let explicit = position("(explicit)");
        let generated = position("(section)");
        assert!(!explicit.is_empty() && !generated.is_empty());
        assert!(explicit.iter().max() < generated.iter().min(), "{items:?}");
        let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(&labels[labels.len() - 2..], ["_audience", "_later"]);
    }

    /// Rank comes from the kind of definition, not from a leading `_`: an
    /// explicit `[[_zeta]]` is listed before the generated `_alpha`.
    #[test]
    fn test_xref_completion_rank_ignores_leading_underscore() {
        let at = |kind, line| AnchorLocation {
            file: PathBuf::from("/a.adoc"),
            line,
            kind,
        };
        let ws = ws_with_anchors(vec![
            ("_alpha", at(AnchorKind::ImplicitSection, 1)),
            ("beta", at(AnchorKind::Explicit, 2)),
            ("_zeta", at(AnchorKind::Explicit, 3)),
        ]);
        let items = resolve_xref_completions(
            "",
            Path::new("/a.adoc"),
            Some(&ws),
            ">>",
            AT_LINE_START,
            None,
        );
        let got: Vec<_> = items
            .iter()
            .map(|item| (item.label.as_str(), item.sort_text.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                ("_zeta", Some("0_zeta")),
                ("beta", Some("0beta")),
                ("_alpha", Some("1_alpha"))
            ]
        );
    }

    /// Of an ID defined twice, the explicit definition is listed whatever
    /// the order of the input: its detail (title) is the one shown.
    #[test]
    fn test_anchor_items_keep_the_explicit_definition_of_a_repeated_id() {
        let at = |file: &str, line, kind| {
            (
                "_x".to_string(),
                AnchorLocation {
                    file: PathBuf::from(file),
                    line,
                    kind,
                },
            )
        };
        // Rank and file order disagree: the implicit one comes first by file.
        let implicit = at("/a.adoc", 1, AnchorKind::ImplicitSection);
        let explicit = at("/b.adoc", 3, AnchorKind::Explicit);
        let ws = ws_with_anchors(vec![]);
        let edit = AnchorEdit {
            closing: ">>",
            span: AT_LINE_START,
            trailing: None,
        };
        for anchors in [
            [implicit.clone(), explicit.clone()],
            [explicit.clone(), implicit.clone()],
        ] {
            let items = anchor_items(&anchors, &ws, edit);
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].detail.as_deref(), Some("b.adoc (explicit)"));
        }
    }

    /// Of an ID with two explicit definitions, the first by file and line
    /// is listed whatever the order of the input.
    #[test]
    fn test_anchor_items_keep_the_first_of_two_explicit_definitions() {
        let at = |file: &str, line| {
            (
                "x".to_string(),
                AnchorLocation {
                    file: PathBuf::from(file),
                    line,
                    kind: AnchorKind::Explicit,
                },
            )
        };
        let later = at("/b.adoc", 1);
        let first = at("/a.adoc", 5);
        let ws = ws_with_anchors(vec![]);
        let edit = AnchorEdit {
            closing: ">>",
            span: AT_LINE_START,
            trailing: None,
        };
        for anchors in [
            [later.clone(), first.clone()],
            [first.clone(), later.clone()],
        ] {
            let items = anchor_items(&anchors, &ws, edit);
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].detail.as_deref(), Some("a.adoc (explicit)"));
        }
    }

    /// The merged list for `xref:` is in the order a client sorts it: by
    /// `sortText`, else by label. Path items carry no `sortText`.
    #[test]
    fn test_xref_or_path_completions_follow_sort_text() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a.adoc");
        std::fs::write(&a, "= A\n").unwrap();
        std::fs::write(dir.path().join("b.adoc"), "= B\n").unwrap();
        let at = |kind, line| AnchorLocation {
            file: a.clone(),
            line,
            kind,
        };
        let ws = ws_with_anchors_at(
            dir.path(),
            vec![
                ("_alpha", at(AnchorKind::ImplicitSection, 1)),
                ("zed", at(AnchorKind::Explicit, 2)),
            ],
        );
        let ctx = CompletionContext::XrefAnchorOrPath {
            prefix: String::new(),
            closing: "[]",
            span: AT_LINE_START,
            trailing: None,
        };
        let items = resolve_completions(ctx, &a, 1, Some(&ws));

        let key = |item: &CompletionItem| item.sort_text.clone().unwrap_or(item.label.clone());
        let keys: Vec<_> = items.iter().map(key).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
        let index = |wanted: &str| labels.iter().position(|l| *l == wanted).unwrap();
        assert!(index("zed") < index("_alpha"), "{labels:?}");
        assert!(labels.contains(&"b.adoc"), "{labels:?}");
    }

    /// `book.adoc` includes `reference.adoc`, so after `reference.adoc#`
    /// Asciidoctor resolves an ID in the whole book: offered are the IDs the
    /// book defines in `reference.adoc` (`_details_2`, not `_details` and
    /// not `shared`, which the book defines in `setup.adoc`, where a link
    /// to them would jump). From `solo/main.adoc` the file is another
    /// document, rendered on its own: its own IDs are offered.
    #[test]
    fn test_xref_cross_completions_offer_the_ids_defined_in_the_file() {
        let book = fixture_labels("book.adoc", "xref:chapters/reference.adoc#");
        assert!(book.contains(&"_details_2".to_string()), "{book:?}");
        for not_here in ["_details", "shared", "setup-only"] {
            assert!(!book.contains(&not_here.to_string()), "{book:?}");
        }

        let solo = fixture_labels("solo/main.adoc", "xref:../chapters/reference.adoc#");
        for here in ["shared", "_details"] {
            assert!(solo.contains(&here.to_string()), "{solo:?}");
        }
        for not_here in ["_details_2", "setup-only"] {
            assert!(!solo.contains(&not_here.to_string()), "{solo:?}");
        }
    }

    /// Another document is rendered on its own, and Asciidoctor links to
    /// every ID it defines, in its included chapters too (goto and the
    /// diagnostics accept them): from `solo/main.adoc`, `xref:../book.adoc#`
    /// offers the `setup-only` that `book.adoc` defines in `setup.adoc`.
    #[test]
    fn test_xref_cross_completions_offer_every_id_of_another_document() {
        let labels = fixture_labels("solo/main.adoc", "xref:../book.adoc#");
        assert!(labels.contains(&"setup-only".to_string()), "{labels:?}");
    }

    /// `book.adoc` includes `a/intro.adoc` and `b/intro.adoc`: a file that
    /// exists is matched by path, so the basename does not offer the IDs of
    /// the other `intro.adoc`.
    #[test]
    fn test_xref_cross_completions_match_an_existing_file_by_path_not_basename() {
        let labels = labels_in(
            fixture("anchor_completion"),
            "book.adoc",
            "xref:a/intro.adoc#",
        );
        assert_eq!(labels, vec!["only-a"]);
    }

    /// `solo/setup.adoc` does not exist; go-to-definition and the diagnostics
    /// take the indexed file whose path ends in `setup.adoc`
    /// (`chapters/setup.adoc`), so completion offers its IDs too.
    #[test]
    fn test_xref_cross_completions_find_a_file_that_is_not_at_the_typed_path() {
        let labels = fixture_labels("solo/main.adoc", "xref:setup.adoc#");

        assert!(labels.contains(&"setup-only".to_string()), "{labels:?}");
    }

    // --- resolve_xref_path_completions ---

    /// Asciidoctor resolves xref paths in a book relative to the `docdir`
    /// of the top-level document, also in an included chapter: from
    /// `chapters/setup.adoc`, `xref:` lists the folder of `index.adoc`
    /// (book scope) and the chapter's own folder (preview scope).
    #[test]
    fn test_xref_path_in_included_file_lists_the_top_level_document_folder() {
        let labels = labels_in(
            fixture("adversarial_project"),
            "chapters/setup.adoc",
            "xref:",
        );
        for expected in ["chapters/", "index.adoc", "reference.adoc", "ref-options"] {
            assert!(labels.contains(&expected.to_string()), "{labels:?}");
        }
    }

    #[test]
    fn test_xref_path_in_included_file_completes_below_the_top_level_folder() {
        let (root, ws) = fixture("adversarial_project");
        let doc = root.join("chapters/setup.adoc");
        let typed = "xref:chapters/";
        let ctx = detect_completion_context(typed, Position::new(0, 14)).unwrap();
        let items = resolve_completions(ctx, &doc, 1, Some(&ws));
        let reference = items.iter().find(|i| i.label == "reference.adoc").unwrap();
        assert_eq!(
            new_text(reference),
            "chapters/reference.adoc${1}[${2:label}]"
        );
    }

    #[test]
    fn test_xref_without_path_offers_folders_and_anchors_of_the_document() {
        let labels = labels_in(fixture("adversarial_project"), "index.adoc", "xref:");
        for expected in ["chapters/", "intro", "setup-install"] {
            assert!(labels.contains(&expected.to_string()), "{labels:?}");
        }
        assert!(
            !labels.contains(&"reference.adoc".to_string()),
            "{labels:?}"
        );
    }

    #[test]
    fn test_xref_path_offers_only_adoc_files() {
        let snippets = labels_in(
            fixture("adversarial_project"),
            "index.adoc",
            "xref:snippets/",
        );
        assert!(
            !snippets.contains(&"config.yml".to_string()),
            "{snippets:?}"
        );
        let images = labels_in(fixture("adversarial_project"), "index.adoc", "xref:images/");
        assert!(images.contains(&"diagrams/".to_string()), "{images:?}");
        assert!(!images.contains(&"logo.png".to_string()), "{images:?}");
    }

    #[test]
    fn test_xref_path_in_a_standalone_document_lists_its_own_folder() {
        let labels = labels_in(anchor_scope_fixture(), "solo/main.adoc", "xref:");
        for expected in ["foreign.adoc", "mixed.adoc"] {
            assert!(labels.contains(&expected.to_string()), "{labels:?}");
        }
        assert!(!labels.contains(&"book.adoc".to_string()), "{labels:?}");
    }

    #[test]
    fn test_xref_path_from_an_included_file_chains_into_its_anchors() {
        let labels = labels_in(
            fixture("adversarial_project"),
            "chapters/setup.adoc",
            "xref:chapters/reference.adoc#",
        );
        assert!(labels.contains(&"ref-options".to_string()), "{labels:?}");
    }

    #[test]
    fn test_xref_path_lists_adoc_siblings() {
        let dir = setup_include_dir();
        let doc = dir.path().join("a.adoc");
        let items = resolve_xref_path_completions("", &doc, 1, None, "[]", AT_LINE_START);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"a.adoc"));
        assert!(labels.contains(&"b.adoc"));
        assert!(labels.contains(&"chapters/"));
        // Files get xref-snippet suffix: cursor before `[`, Tab → label slot
        let b_item = items.iter().find(|i| i.label == "b.adoc").unwrap();
        assert_eq!(new_text(b_item), "b.adoc${1}[${2:label}]");
        assert_eq!(b_item.insert_text_format, Some(InsertTextFormat::SNIPPET));
    }

    #[test]
    fn test_xref_path_lists_files_in_subdirectory() {
        let dir = setup_include_dir();
        let doc = dir.path().join("a.adoc");
        // `See xref:chapters/` with the cursor at the end of the line
        let typed = span(9, 18);
        let items = resolve_xref_path_completions("chapters/", &doc, 1, None, "[]", typed);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["intro.adoc"]);
        let intro = &items[0];
        assert_eq!(edit(intro).range, cols(9, 18));
        assert_eq!(new_text(intro), "chapters/intro.adoc${1}[${2:label}]");
        assert_eq!(intro.insert_text, None);
        assert_eq!(intro.filter_text.as_deref(), Some("chapters/intro.adoc"));
    }

    #[test]
    fn test_xref_path_replaces_partly_typed_file_name() {
        let dir = setup_include_dir();
        let doc = dir.path().join("a.adoc");
        // `xref:chapters/in` with the cursor at the end of the line
        let typed = span(5, 16);
        let items = resolve_xref_path_completions("chapters/in", &doc, 1, None, "[]", typed);
        assert_eq!(items.len(), 1);
        assert_eq!(edit(&items[0]).range, cols(5, 16));
        assert_eq!(new_text(&items[0]), "chapters/intro.adoc${1}[${2:label}]");
    }

    #[test]
    fn test_xref_path_folder_replaces_typed_prefix() {
        let dir = setup_include_dir();
        let doc = dir.path().join("a.adoc");
        // `xref:chap` with the cursor at the end of the line
        let typed = span(5, 9);
        let items = resolve_xref_path_completions("chap", &doc, 1, None, "[]", typed);
        let folder = items.iter().find(|i| i.label == "chapters/").unwrap();
        assert_eq!(edit(folder).range, cols(5, 9));
        assert_eq!(new_text(folder), "chapters/");
        assert_eq!(folder.insert_text, None);
    }

    #[test]
    fn test_xref_items_replace_from_after_the_macro_utf16() {
        let dir = setup_include_dir();
        let doc = dir.path().join("a.adoc");
        let ws = ws_with_anchors_at(
            dir.path(),
            vec![(
                "_goals",
                AnchorLocation {
                    file: dir.path().join("a.adoc"),
                    line: 1,
                    kind: AnchorKind::Explicit,
                },
            )],
        );
        // `Ü` is two bytes but one UTF-16 unit: the path starts at column 10.
        let content = "Über xref:chapters/";
        let ctx = detect_completion_context(content, Position::new(0, 19)).unwrap();
        let items = resolve_completions(ctx, &doc, 1, Some(&ws));
        let intro = items.iter().find(|i| i.label == "intro.adoc").unwrap();
        assert_eq!(
            edit(intro).range,
            Range::new(Position::new(0, 10), Position::new(0, 19))
        );

        let content = "Über xref:_go";
        let ctx = detect_completion_context(content, Position::new(0, 13)).unwrap();
        let items = resolve_completions(ctx, &doc, 1, Some(&ws));
        let goals = items.iter().find(|i| i.label == "_goals").unwrap();
        assert_eq!(
            edit(goals).range,
            Range::new(Position::new(0, 10), Position::new(0, 13))
        );
        assert_eq!(new_text(goals), "_goals[${1:label}]");
    }

    #[test]
    fn test_xref_items_before_existing_brackets_differ_in_range() {
        let dir = setup_include_dir();
        let doc = dir.path().join("a.adoc");
        let ws = ws_with_anchors_at(
            dir.path(),
            vec![(
                "b_anchor",
                AnchorLocation {
                    file: dir.path().join("a.adoc"),
                    line: 1,
                    kind: AnchorKind::Explicit,
                },
            )],
        );
        // `xref:b|[Text]`: the path item keeps the brackets and replaces
        // only up to the cursor; the anchor item swallows them and
        // re-emits the label slot.
        let content = "xref:b[Text]";
        let ctx = detect_completion_context(content, Position::new(0, 6)).unwrap();
        let items = resolve_completions(ctx, &doc, 1, Some(&ws));
        let path = items.iter().find(|i| i.label == "b.adoc").unwrap();
        assert_eq!(edit(path).range, cols(5, 6));
        assert_eq!(new_text(path), "b.adoc");
        let anchor = items.iter().find(|i| i.label == "b_anchor").unwrap();
        assert_eq!(edit(anchor).range, cols(5, 12));
        assert_eq!(new_text(anchor), "b_anchor[${1:label}]");
    }

    #[test]
    fn test_cross_file_anchor_items_replace_after_the_hash_utf16() {
        let dir = setup_include_dir();
        let doc = dir.path().join("a.adoc");
        let ws = ws_with_anchors(vec![(
            "_goals",
            AnchorLocation {
                file: dir.path().join("chapters/intro.adoc"),
                line: 1,
                kind: AnchorKind::Explicit,
            },
        )]);
        // `Ü` is two bytes but one UTF-16 unit.
        let content = "Über xref:chapters/intro.adoc#_go";
        let ctx = detect_completion_context(content, Position::new(0, 33)).unwrap();
        let items = resolve_completions(ctx, &doc, 1, Some(&ws));
        let goals = items.iter().find(|i| i.label == "_goals").unwrap();
        assert_eq!(edit(goals).range, cols(30, 33));
        assert_eq!(new_text(goals), "_goals[${1:label}]");

        let content = "Über <<chapters/intro.adoc#_go";
        let ctx = detect_completion_context(content, Position::new(0, 30)).unwrap();
        let items = resolve_completions(ctx, &doc, 1, Some(&ws));
        let goals = items.iter().find(|i| i.label == "_goals").unwrap();
        assert_eq!(edit(goals).range, cols(27, 30));
        assert_eq!(new_text(goals), "_goals>>");
    }

    #[test]
    fn test_inline_image_items_replace_from_after_the_macro_utf16() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/pic.png"), b"").unwrap();
        let doc = dir.path().join("doc.adoc");
        let content = "Ü image:sub/";
        let ctx = detect_completion_context(content, Position::new(0, 12)).unwrap();
        let items = resolve_completions(ctx, &doc, 1, None);
        let pic = items.iter().find(|i| i.label == "pic.png").unwrap();
        assert_eq!(edit(pic).range, cols(8, 12));
        assert_eq!(new_text(pic), "sub/pic.png[${1:alt}]");
    }

    // --- resolve_include_completions ---

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
        let items = resolve_include_completions("", &doc_file, 1, None, AT_LINE_START);
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
        // edit text for files uses include-snippet (cursor inside `[opts]`)
        assert_eq!(new_text(a_item), "a.adoc[${1:opts}]");
        assert_eq!(a_item.insert_text_format, Some(InsertTextFormat::SNIPPET));
        // Directories have Folder kind
        let ch_item = items.iter().find(|i| i.label == "chapters/").unwrap();
        assert_eq!(ch_item.kind, Some(CompletionItemKind::FOLDER));
        // edit text for dirs is the path so far (no suffix snippet)
        assert_eq!(new_text(ch_item), "chapters/");
    }

    #[test]
    fn test_include_completions_prefix_filter() {
        let dir = setup_include_dir();
        let doc_file = dir.path().join("dummy.adoc");
        let items = resolve_include_completions("chap", &doc_file, 1, None, AT_LINE_START);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "chapters/");
    }

    #[test]
    fn test_include_completions_subdir() {
        let dir = setup_include_dir();
        let doc_file = dir.path().join("dummy.adoc");
        // `include::chapters/` with the cursor at the end of the line
        let typed = span(9, 18);
        let items = resolve_include_completions("chapters/", &doc_file, 1, None, typed);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "intro.adoc");
        assert_eq!(edit(&items[0]).range, cols(9, 18));
        assert_eq!(new_text(&items[0]), "chapters/intro.adoc[${1:opts}]");
    }

    // --- resolve_include_completions with attribute substitution ---

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
            HashMap::new(),
            HashMap::new(),
        );
        (dir, ws, doc_file)
    }

    #[test]
    fn test_include_completions_with_resolved_attr() {
        let (_dir, ws, doc_file) = setup_include_attr_dir();
        // `include::{source-dir}/` with the cursor at the end of the line
        let typed = span(9, 22);
        let items = resolve_include_completions("{source-dir}/", &doc_file, 1, Some(&ws), typed);
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
        // edit text preserves original {source-dir}/ prefix
        let a_item = items.iter().find(|i| i.label == "module-a.adoc").unwrap();
        assert_eq!(edit(a_item).range, cols(9, 22));
        assert_eq!(new_text(a_item), "{source-dir}/module-a.adoc[${1:opts}]");
    }

    #[test]
    fn test_include_completions_with_unresolved_attr() {
        let (_dir, ws, doc_file) = setup_include_attr_dir();
        let items =
            resolve_include_completions("{unknown}/", &doc_file, 1, Some(&ws), AT_LINE_START);
        assert!(items.is_empty(), "expected empty, got {:?}", items);
    }

    #[test]
    fn test_include_completions_attr_with_filter() {
        let (_dir, ws, doc_file) = setup_include_attr_dir();
        let items = resolve_include_completions(
            "{source-dir}/module-a",
            &doc_file,
            1,
            Some(&ws),
            AT_LINE_START,
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "module-a.adoc");
        assert_eq!(new_text(&items[0]), "{source-dir}/module-a.adoc[${1:opts}]");
    }

    #[test]
    fn test_include_completions_no_attr_with_workspace() {
        let (_dir, ws, doc_file) = setup_include_attr_dir();
        // No attribute in prefix — should work like before, even with workspace present
        let items = resolve_include_completions("src/", &doc_file, 1, Some(&ws), AT_LINE_START);
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

    // --- Completion over several attribute scopes (ADR-4) ---

    fn labels(items: &[CompletionItem]) -> Vec<&str> {
        items.iter().map(|i| i.label.as_str()).collect()
    }

    /// Two top-level documents point `{edition}` at different folders: the
    /// candidates of both are offered.
    #[test]
    fn test_include_completions_offer_union_of_top_level_documents() {
        let (_dir, ws, paths) = scanned(&[
            ("a-print.adoc", "= A\n:edition: print\ninclude::ch.adoc[]\n"),
            ("b-web.adoc", "= B\n:edition: web\ninclude::ch.adoc[]\n"),
            ("ch.adoc", "== C\n\ninclude::{edition}/\n"),
            ("print/p.adoc", ""),
            ("web/w.adoc", ""),
        ]);
        let items =
            resolve_include_completions("{edition}/", &paths[2], 3, Some(&ws), AT_LINE_START);
        assert_eq!(labels(&items), vec!["p.adoc", "w.adoc"]);
    }

    // --- Include targets: any text file, not hidden, not the document itself ---

    fn adversarial_labels(file: &str, typed: &str) -> Vec<String> {
        labels_in(fixture("adversarial_project"), file, typed)
    }

    /// Asciidoctor includes any text file (`[source,yaml]` + `include::config.yml[]`).
    #[test]
    fn test_include_completions_offer_non_adoc_files() {
        assert_eq!(
            adversarial_labels("chapters/setup.adoc", "include::../snippets/"),
            vec!["config.yml"]
        );
        let (root, ws) = fixture("adversarial_project");
        let typed = "include::../snippets/";
        let ctx = detect_completion_context(typed, Position::new(0, 21)).unwrap();
        let items = resolve_completions(ctx, &root.join("chapters/setup.adoc"), 1, Some(&ws));
        assert_eq!(new_text(&items[0]), "../snippets/config.yml[${1:opts}]");
    }

    #[test]
    fn test_include_completions_skip_the_current_file() {
        let labels = adversarial_labels("chapters/setup.adoc", "include::");
        assert!(labels.contains(&"intro.adoc".to_string()), "{labels:?}");
        assert!(labels.contains(&"reference.adoc".to_string()), "{labels:?}");
        assert!(!labels.contains(&"setup.adoc".to_string()), "{labels:?}");
    }

    #[test]
    fn test_include_completions_skip_the_current_file_reached_through_an_attribute() {
        let labels = adversarial_labels(
            "chapters/setup.adoc",
            "include::{asciidoctorconfigdir}/chapters/",
        );
        assert!(labels.contains(&"intro.adoc".to_string()), "{labels:?}");
        assert!(!labels.contains(&"setup.adoc".to_string()), "{labels:?}");
    }

    #[test]
    fn test_include_completions_skip_hidden_files() {
        let labels = adversarial_labels("index.adoc", "include::");
        assert!(
            !labels.contains(&".asciidoctorconfig.adoc".to_string()),
            "{labels:?}"
        );
        assert!(labels.contains(&"chapters/".to_string()), "{labels:?}");
        assert!(labels.contains(&"snippets/".to_string()), "{labels:?}");
        assert!(adversarial_labels("index.adoc", "include::.asc").is_empty());
    }

    /// `logo.png` has no NUL byte but is not UTF-8, which Asciidoctor
    /// cannot include either.
    #[test]
    fn test_include_completions_skip_binary_files() {
        assert_eq!(
            adversarial_labels("chapters/setup.adoc", "include::../images/diagrams/"),
            vec!["arch.svg"]
        );
        let labels = adversarial_labels("chapters/setup.adoc", "include::../images/");
        assert!(labels.contains(&"diagrams/".to_string()), "{labels:?}");
        assert!(!labels.contains(&"logo.png".to_string()), "{labels:?}");
    }

    /// `asciidoctorconfigdir` is an editor convention the Asciidoctor CLI
    /// does not know (it reports "include file not found"); completion
    /// follows the config chain (ADR-008).
    #[test]
    fn test_include_completions_resolve_attribute_paths() {
        assert_eq!(
            adversarial_labels(
                "chapters/setup.adoc",
                "include::{asciidoctorconfigdir}/partials/"
            ),
            vec!["legal.adoc"]
        );
        assert_eq!(
            adversarial_labels(
                "chapters/setup.adoc",
                "include::{asciidoctorconfigdir}/snippets/"
            ),
            vec!["config.yml"]
        );
    }

    /// Self-exclusion compares canonical paths, not file names: a same-named
    /// file in another folder stays a valid include target.
    #[test]
    fn test_include_completions_skip_only_the_same_file_not_the_same_name() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        std::fs::create_dir(dir.path().join("b")).unwrap();
        std::fs::write(dir.path().join("a/x.adoc"), "= A\n").unwrap();
        std::fs::write(dir.path().join("b/x.adoc"), "= B\n").unwrap();
        let doc = dir.path().join("a/x.adoc");
        let own = resolve_include_completions("", &doc, 1, None, AT_LINE_START);
        assert!(!labels(&own).contains(&"x.adoc"), "{:?}", labels(&own));
        let other = resolve_include_completions("../b/", &doc, 1, None, AT_LINE_START);
        assert_eq!(labels(&other), vec!["x.adoc"]);
    }

    /// Only regular files are sniffed: opening a FIFO without a writer
    /// blocks forever, which would freeze the server.
    #[cfg(unix)]
    #[test]
    fn test_include_completions_do_not_open_fifos() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.adoc"), "= A\n").unwrap();
        let fifo = dir.path().join("pipe");
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        let doc = dir.path().join("dummy.adoc");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let items = resolve_include_completions("", &doc, 1, None, AT_LINE_START);
            let _ = tx.send(items.into_iter().map(|i| i.label).collect::<Vec<_>>());
        });
        let labels = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("completion blocked on a FIFO");
        assert_eq!(labels, vec!["a.adoc"]);
    }

    /// The include filters must not leak into xref: it targets `.adoc`
    /// files only, and a self-xref is valid.
    #[test]
    fn test_xref_path_completions_keep_adoc_only_and_the_current_file() {
        assert!(adversarial_labels("index.adoc", "xref:snippets/").is_empty());
        let labels = adversarial_labels("index.adoc", "xref:");
        assert!(labels.contains(&"index.adoc".to_string()), "{labels:?}");
    }

    #[rstest]
    #[case(b"", true)]
    #[case(b"key: value\n", true)]
    #[case("ä".as_bytes(), true)]
    #[case(&"ä".as_bytes()[..1], true)]
    #[case(b"\x89PNG\r\n\x1a\n", false)]
    #[case(b"a\0b", false)]
    #[case(b"a\xffb", false)]
    fn test_looks_like_text(#[case] head: &[u8], #[case] expected: bool) {
        assert_eq!(looks_like_text(head), expected);
    }

    /// An image folder set by one top-level document and another by the
    /// chapter's own preview: both are listed.
    #[test]
    fn test_image_completions_offer_union_with_preview() {
        let (_dir, ws, paths) = scanned(&[
            ("book.adoc", "= B\n:imagesdir: img\ninclude::ch.adoc[]\n"),
            ("ch.adoc", "== C\n\nimage::\n"),
            ("img/book.png", ""),
            ("own.png", ""),
        ]);
        let items = resolve_image_completions("", &paths[1], 3, Some(&ws), AT_LINE_START);
        let labels = labels(&items);
        assert!(labels.contains(&"book.png"), "{labels:?}");
        assert!(labels.contains(&"own.png"), "{labels:?}");
    }

    /// Attribute names come from every top-level document; the value is
    /// shown per document where they differ.
    #[test]
    fn test_attribute_completions_offer_union_of_top_level_documents() {
        let (_dir, ws, paths) = scanned(&[
            (
                "a-draft.adoc",
                "= A\n:x: draft\n:only-a: 1\ninclude::ch.adoc[]\n",
            ),
            ("b-book.adoc", "= B\n:x: book\ninclude::ch.adoc[]\n"),
            ("ch.adoc", "== C\n\n{\n"),
        ]);
        let items = resolve_attribute_completions("", &paths[2], 3, Some(&ws), "}", AT_LINE_START);
        let detail = |name: &str| {
            items
                .iter()
                .find(|i| i.label == name)
                .and_then(|i| i.detail.clone())
        };
        assert_eq!(detail("only-a").as_deref(), Some("1"));
        assert_eq!(
            detail("x").as_deref(),
            Some("a-draft.adoc: draft, b-book.adoc: book")
        );
        assert_eq!(labels(&items).iter().filter(|l| **l == "x").count(), 1);
    }

    // --- resolve_attribute_completions ---

    /// A scanned workspace with `.asciidoctorconfig` and a document
    /// `doc.adoc` that sets `:author: Alice`; returns the document's path.
    fn config_and_author_doc() -> (TempDir, Workspace, PathBuf) {
        let (dir, ws, mut paths) = scanned(&[
            (".asciidoctorconfig", ":imagesdir: images\n"),
            ("doc.adoc", "= Doc\n:author: Alice\n"),
        ]);
        (dir, ws, paths.remove(1))
    }

    /// Accept the item `label` at the `|` in `marked` and return the
    /// document text afterwards.
    fn accept(marked: &str, label: &str) -> String {
        let bar = marked.find('|').expect("cursor marker");
        let content = marked.replace('|', "");
        let before = &marked[..bar];
        let line = u32::try_from(before.matches('\n').count()).unwrap();
        let character = u32::try_from(before.rsplit('\n').next().unwrap().len()).unwrap();
        let (_dir, ws, files) = scanned(&[(
            "doc.adoc",
            "= Doc\n:product: Widget\n[[ref-options]]\n[[setup-install]]\n",
        )]);
        let ctx = detect_completion_context(&content, Position::new(line, character))
            .expect("completion context");
        let items = resolve_completions(ctx, &files[0], 3, Some(&ws));
        let item = items
            .iter()
            .find(|i| i.label == label)
            .unwrap_or_else(|| panic!("no item {label}"));
        let edit = edit(item);
        let offset = |p: Position| -> usize {
            content
                .split_inclusive('\n')
                .take(p.line as usize)
                .map(str::len)
                .sum::<usize>()
                + p.character as usize
        };
        format!(
            "{}{}{}",
            &content[..offset(edit.range.start)],
            edit.new_text,
            &content[offset(edit.range.end)..]
        )
    }

    #[rstest]
    #[case(
        "attribute at name end before closing",
        "Use {pro|} now.",
        "product",
        "Use {product} now."
    )]
    #[case(
        "attribute mid-name before closing",
        "Use {pr|o} now.",
        "product",
        "Use {product} now."
    )]
    #[case(
        "attribute without closing",
        "Use {pro| now.",
        "product",
        "Use {product} now."
    )]
    #[case(
        "attribute with later unrelated closing",
        "Use {pro| and {x}",
        "product",
        "Use {product} and {x}"
    )]
    #[case(
        "attribute with later closing after space",
        "Use {pro| now}",
        "product",
        "Use {product} now}"
    )]
    #[case(
        "xref shorthand mid-token",
        "<<|ref-o>>",
        "ref-options",
        "<<ref-options>>"
    )]
    #[case(
        "xref shorthand before comma",
        "<<setup-install|,the install>>",
        "setup-install",
        "<<setup-install,the install>>"
    )]
    #[case(
        "xref shorthand fresh",
        "See <<ref-|",
        "ref-options",
        "See <<ref-options>>"
    )]
    #[case(
        "xref shorthand text on next line",
        "<<setup-install|,the\ninstall>> here",
        "setup-install",
        "<<setup-install,the\ninstall>> here"
    )]
    #[case(
        "xref shorthand ignores later xref across lines",
        "<<ref-|\n<<b>>",
        "ref-options",
        "<<ref-options>>\n<<b>>"
    )]
    #[case(
        "xref shorthand stops at blank line",
        "<<ref-|\n\nx>>",
        "ref-options",
        "<<ref-options>>\n\nx>>"
    )]
    #[case("ifdef name", "ifdef::pro|", "product", "ifdef::product")]
    #[case(
        "ifdef mid-name before brackets",
        "ifdef::pr|o[]",
        "product",
        "ifdef::product[]"
    )]
    #[case(
        "ifndef name before brackets",
        "ifndef::pro|[]",
        "product",
        "ifndef::product[]"
    )]
    #[case(
        "ifdef second name after comma",
        "ifdef::x,pro|",
        "product",
        "ifdef::x,product"
    )]
    #[case(
        "ifdef keeps later names",
        "ifdef::pro|,x[]",
        "product",
        "ifdef::product,x[]"
    )]
    #[case(
        "ifdef second name after plus",
        "ifdef::x+pro|[]",
        "product",
        "ifdef::x+product[]"
    )]
    fn test_accepting_completion_yields_wellformed_text(
        #[case] _name: &str,
        #[case] marked: &str,
        #[case] label: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(accept(marked, label), expected);
    }

    #[test]
    fn test_ifdef_completion_offers_sectnums_in_fixture() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/adversarial_project")
            .canonicalize()
            .unwrap();
        let index = root.join("index.adoc");
        let ws = Workspace::build(&root, &index);
        let file = root.join("chapters/setup.adoc");
        let mut content = std::fs::read_to_string(&file).unwrap();
        let line = content.lines().count();
        content.push_str("ifdef::sectn");
        let pos = Position::new(u32::try_from(line).unwrap(), 12);
        let ctx = detect_completion_context(&content, pos).expect("completion context");

        let items = resolve_completions(ctx, &file, line, Some(&ws));

        let item = items
            .iter()
            .find(|i| i.label == "sectnums")
            .expect("sectnums offered");
        assert_eq!(new_text(item), "sectnums");
    }

    #[test]
    fn test_attribute_completions_basic() {
        let (_dir, ws, file) = config_and_author_doc();

        let items = resolve_attribute_completions("", &file, 1, Some(&ws), "}", AT_LINE_START);
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
        // Check the edit appends closing }
        assert_eq!(new_text(author), "author}");
    }

    #[test]
    fn test_attribute_completions_omit_closing_when_one_follows() {
        let (_dir, ws, file) = config_and_author_doc();

        let items = resolve_attribute_completions("auth", &file, 1, Some(&ws), "", AT_LINE_START);
        assert_eq!(new_text(&items[0]), "author");
    }

    #[test]
    fn test_attribute_completions_prefix_filter() {
        let (_dir, ws, file) = config_and_author_doc();

        let items = resolve_attribute_completions("im", &file, 1, Some(&ws), "}", AT_LINE_START);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "imagesdir");
    }

    #[test]
    fn test_attribute_completions_no_workspace() {
        let file = Path::new("/tmp/doc.adoc");
        let items = resolve_attribute_completions("", file, 1, None, "}", AT_LINE_START);
        assert!(items.is_empty());
    }

    // --- resolve_completions (dispatcher) ---

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
            span: AT_LINE_START,
            trailing: None,
        };

        let items = resolve_completions(ctx, doc_path, 1, Some(&ws));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "my-anchor");
    }

    #[test]
    fn test_resolve_completions_include() {
        let dir = setup_include_dir();
        // doc_path is a file inside the dir (as in real LSP usage); not one
        // of the listed files, since a document is no include target for itself
        let doc_file = dir.path().join("dummy.adoc");
        let ctx = CompletionContext::IncludePath {
            prefix: "".into(),
            span: AT_LINE_START,
        };

        let items = resolve_completions(ctx, &doc_file, 1, None);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"a.adoc"));
        assert!(labels.contains(&"b.adoc"));
        assert!(labels.contains(&"chapters/"));
    }

    #[test]
    fn test_resolve_completions_attribute() {
        let (_dir, ws, file) = config_and_author_doc();
        let ctx = CompletionContext::Attribute {
            prefix: "im".into(),
            span: AT_LINE_START,
            closing: "}",
        };

        let items = resolve_completions(ctx, &file, 1, Some(&ws));
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
                span: span(5, 11),
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
        let items = resolve_completions(ctx, doc_path, 1, None);
        assert!(!items.is_empty());
        assert_eq!(items[0].label, "image::");
    }

    #[test]
    fn test_dispatch_admonition() {
        let doc_path = Path::new("/tmp/doc.adoc");
        let ctx = CompletionContext::Admonition {
            prefix: "NO".into(),
        };
        let items = resolve_completions(ctx, doc_path, 1, None);
        assert!(!items.is_empty());
        assert_eq!(items[0].label, "NOTE:");
    }

    #[test]
    fn test_dispatch_doc_attribute() {
        let doc_path = Path::new("/tmp/doc.adoc");
        let ctx = CompletionContext::DocumentAttribute {
            prefix: "to".into(),
        };
        let items = resolve_completions(ctx, doc_path, 1, None);
        assert!(!items.is_empty());
        assert_eq!(items[0].label, "toc");
    }

    #[test]
    fn test_dispatch_inline_macro() {
        let doc_path = Path::new("/tmp/doc.adoc");
        let ctx = CompletionContext::InlineMacro {
            macro_name: "kbd".into(),
        };
        let items = resolve_completions(ctx, doc_path, 1, None);
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
        let items = resolve_completions(ctx, doc_path, 1, None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "kbd:");
    }

    // --- Attribute value completions ---

    #[test]
    fn test_attribute_value_completions_offer_toc_choices() {
        let items = resolve_attribute_value_completions("toc", "", span(6, 6));
        assert_eq!(labels(&items), ["auto", "left", "right", "preamble"]);
        for item in &items {
            assert_eq!(item.kind, Some(CompletionItemKind::ENUM_MEMBER));
            assert_eq!(item.detail.as_deref(), Some("Table of contents placement"));
            assert_eq!(edit(item).new_text, item.label);
            assert_eq!(edit(item).range, cols(6, 6));
        }
    }

    #[test]
    fn test_attribute_value_completions_filter_by_prefix() {
        let items = resolve_attribute_value_completions("toc", "le", span(6, 8));
        assert_eq!(labels(&items), ["left"]);
        assert_eq!(edit(&items[0]).range, cols(6, 8));
    }

    #[test]
    fn test_attribute_value_edit_covers_the_token_after_the_cursor() {
        let items = resolve_attribute_value_completions("toc", "le", span_to(6, 8, 10));
        assert_eq!(edit(&items[0]).range, cols(6, 10));
    }

    #[test]
    fn test_attribute_value_completions_keep_the_dot() {
        let items =
            resolve_attribute_value_completions("source-highlighter", "highlight.", span(21, 31));
        assert_eq!(labels(&items), ["highlight.js"]);
        assert_eq!(edit(&items[0]).new_text, "highlight.js");
    }

    #[test]
    fn test_block_style_prefix_case_insensitive() {
        let items = resolve_block_style_completions("no", span(1, 3));
        let found = labels(&items);
        assert!(found.contains(&"NOTE"), "labels: {found:?}");
        assert!(found.contains(&"normal"), "labels: {found:?}");
        assert!(!found.contains(&"TIP"), "labels: {found:?}");
    }

    #[test]
    fn test_block_style_empty_prefix_lists_catalog() {
        let items = resolve_block_style_completions("", span(1, 1));
        assert_eq!(items.len(), BLOCK_STYLES.len());
        let mut found = labels(&items);
        found.sort_unstable();
        found.dedup();
        assert_eq!(found.len(), BLOCK_STYLES.len(), "one item per name");
    }

    #[test]
    fn test_block_style_edit_replaces_token_only() {
        let items = resolve_block_style_completions("NO", span_to(1, 3, 5));
        let note = items.iter().find(|i| i.label == "NOTE").unwrap();
        assert_eq!(edit(note).range, cols(1, 5));
        assert_eq!(edit(note).new_text, "NOTE");
        assert_eq!(note.kind, Some(CompletionItemKind::KEYWORD));
    }

    #[test]
    fn test_source_language_rust() {
        let items = resolve_source_language_completions("ru", span(8, 10));
        let rust = items.iter().find(|i| i.label == "rust").unwrap();
        assert_eq!(edit(rust).new_text, "rust");
        assert_eq!(edit(rust).range, cols(8, 10));
        assert_eq!(rust.kind, Some(CompletionItemKind::VALUE));
    }

    #[test]
    fn test_source_language_unknown_prefix_empty() {
        assert!(resolve_source_language_completions("zzz", span(8, 11)).is_empty());
    }

    #[test]
    fn test_attribute_value_completions_are_plain_text() {
        let items = resolve_attribute_value_completions("toc", "", span(6, 6));
        assert!(items.iter().all(|i| i.insert_text_format.is_none()));
    }

    #[test]
    fn test_attribute_value_verify_toc() {
        let ctx = detect_completion_context(
            ":toc: ",
            Position {
                line: 0,
                character: 6,
            },
        )
        .expect("context after `:toc: `");
        let items = resolve_completions(ctx, Path::new("/tmp/doc.adoc"), 1, None);
        let labels = labels(&items);
        for expected in ["auto", "left", "right", "preamble"] {
            assert!(labels.contains(&expected), "missing {expected}: {labels:?}");
        }
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
    // image:: path detection
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
                prefix: "foto".into(),
                span: span(7, 11),
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
            Some(CompletionContext::ImagePath {
                prefix: "".into(),
                span: span(7, 7),
            })
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
                prefix: "sub/foto".into(),
                span: span(7, 15),
            })
        );
    }

    // -----------------------------------------------------------------------
    // image:: attribute delegation
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
                prefix: "doc".into(),
                span: span(8, 11),
                closing: "}"
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
                prefix: "{docdir}/foto".into(),
                span: span(7, 20),
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
    // inline image: detection
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
                prefix: "foto".into(),
                span: span(11, 15),
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
            Some(CompletionContext::ImagePath {
                prefix: "".into(),
                span: span(11, 11),
            })
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
                prefix: "doc".into(),
                span: span(7, 10),
                closing: "}"
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
                prefix: "foto".into(),
                span: span(7, 11),
            })
        );
    }

    // --- Image Path Resolution ---

    #[test]
    fn test_resolve_image_empty_dir() {
        let dir = TempDir::new().unwrap();
        let doc_file = dir.path().join("doc.adoc");
        let items = resolve_image_completions("", &doc_file, 1, None, AT_LINE_START);
        assert!(items.is_empty(), "empty dir should yield no completions");
    }

    #[test]
    fn test_resolve_image_lists_images() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("foto.png"), b"").unwrap();
        std::fs::write(dir.path().join("diag.svg"), b"").unwrap();
        std::fs::write(dir.path().join("readme.txt"), b"").unwrap();
        let doc_file = dir.path().join("doc.adoc");

        let items = resolve_image_completions("", &doc_file, 1, None, AT_LINE_START);
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
        // edit text uses image-snippet (cursor inside `[alt]`)
        let png_item = items.iter().find(|i| i.label == "foto.png").unwrap();
        assert_eq!(new_text(png_item), "foto.png[${1:alt}]");
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
            HashMap::new(),
            HashMap::new(),
        );

        let items = resolve_image_completions("", &file, 1, Some(&ws), AT_LINE_START);
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

        // `image::sub/` with the cursor at the end of the line
        let typed = span(7, 11);
        let items = resolve_image_completions("sub/", &doc_file, 1, None, typed);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "icon.gif");
        assert_eq!(edit(&items[0]).range, cols(7, 11));
        assert_eq!(new_text(&items[0]), "sub/icon.gif[${1:alt}]");
    }

    #[test]
    fn test_resolve_image_prefix_filter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("foto.png"), b"").unwrap();
        std::fs::write(dir.path().join("diagram.svg"), b"").unwrap();
        let doc_file = dir.path().join("doc.adoc");

        let items = resolve_image_completions("foto", &doc_file, 1, None, AT_LINE_START);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "foto.png");
    }

    // --- Image Path Dispatch ---

    #[test]
    fn test_resolve_image_no_imagesdir() {
        // ImagePath without workspace → lists relative to document
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("screenshot.png"), b"").unwrap();
        let doc_file = dir.path().join("doc.adoc");

        let ctx = CompletionContext::ImagePath {
            prefix: "".into(),
            span: AT_LINE_START,
        };
        let items = resolve_completions(ctx, &doc_file, 1, None);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"screenshot.png"),
            "missing screenshot.png in {labels:?}"
        );
    }
}
