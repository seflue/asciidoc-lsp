//! Semantic tokens: syntax highlighting computed from the grammar's own
//! highlight queries, for clients without a tree-sitter highlighter.

use std::sync::OnceLock;

use tower_lsp_server::ls_types::{
    SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokensLegend,
};
use tree_sitter_highlight::{Highlight, HighlightConfiguration, HighlightEvent, Highlighter};

/// Token types of the legend, in legend order. Markup has no standard type,
/// so the custom ones (`heading`, `strong`, ...) sit next to the standard
/// ones; the VS Code extension declares them and maps them to `markup.*`
/// scopes.
const TOKEN_TYPES: [&str; 20] = [
    "comment",
    "keyword",
    "property",
    "variable",
    "parameter",
    "type",
    "label",
    "string",
    "regexp",
    "number",
    "heading",
    "strong",
    "emphasis",
    "raw",
    "link",
    "listMarker",
    "subscript",
    "superscript",
    "punctuation",
    "admonition",
];

/// Modifiers of the legend, in legend order (bit position = index).
const TOKEN_MODIFIERS: [&str; 13] = [
    "declaration",
    "readonly",
    "level1",
    "level2",
    "level3",
    "level4",
    "level5",
    "level6",
    "note",
    "warning",
    "error",
    "checked",
    "unchecked",
];

pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: TOKEN_TYPES
            .iter()
            .map(|name| SemanticTokenType::new(name))
            .collect(),
        token_modifiers: TOKEN_MODIFIERS
            .iter()
            .map(|name| SemanticTokenModifier::new(name))
            .collect(),
    }
}

/// How a highlight capture of the grammar's queries appears in the legend:
/// capture name, token type, modifiers. The capture names are the
/// "recognized names" handed to `tree-sitter-highlight`. It resolves a
/// capture to the longest recognized name whose dot-separated parts all
/// occur in the capture name (`markup.raw.block` matches `markup.raw.block`,
/// else `markup.raw`), and to no highlight when none matches.
const STYLES: [(&str, &str, &[&str]); 36] = [
    ("comment", "comment", &[]),
    ("comment.note", "admonition", &["note"]),
    ("comment.warning", "admonition", &["warning"]),
    ("comment.error", "admonition", &["error"]),
    ("markup.heading.1", "heading", &["level1"]),
    ("markup.heading.2", "heading", &["level2"]),
    ("markup.heading.3", "heading", &["level3"]),
    ("markup.heading.4", "heading", &["level4"]),
    ("markup.heading.5", "heading", &["level5"]),
    ("markup.heading.6", "heading", &["level6"]),
    ("markup.strong", "strong", &[]),
    ("markup.italic", "emphasis", &[]),
    ("markup.raw", "raw", &[]),
    ("markup.raw.block", "raw", &[]),
    ("markup.link", "link", &[]),
    ("markup.link.url", "link", &[]),
    ("markup.link.label", "label", &[]),
    ("markup.list", "listMarker", &[]),
    ("markup.list.checked", "listMarker", &["checked"]),
    ("markup.list.unchecked", "listMarker", &["unchecked"]),
    ("markup.subscript", "subscript", &[]),
    ("markup.superscript", "superscript", &[]),
    ("keyword", "keyword", &[]),
    ("property", "property", &["declaration"]),
    ("constant", "variable", &["readonly"]),
    ("attribute", "parameter", &[]),
    ("variable.parameter", "parameter", &[]),
    ("type", "type", &[]),
    ("label", "label", &[]),
    ("string", "string", &[]),
    ("string.special", "string", &[]),
    ("string.escape", "regexp", &[]),
    ("number", "number", &[]),
    ("punctuation.bracket", "punctuation", &[]),
    ("punctuation.delimiter", "punctuation", &[]),
    ("punctuation.special", "punctuation", &[]),
];

const BLOCK_HIGHLIGHTS: &str = include_str!(
    "../../../vendor/tree-sitter-asciidoc/tree-sitter-asciidoc/queries/highlights.scm"
);
const BLOCK_INJECTIONS: &str = include_str!(
    "../../../vendor/tree-sitter-asciidoc/tree-sitter-asciidoc/queries/injections.scm"
);
const INLINE_HIGHLIGHTS: &str = include_str!(
    "../../../vendor/tree-sitter-asciidoc/tree-sitter-asciidoc_inline/queries/highlights.scm"
);
const INLINE_INJECTIONS: &str = include_str!(
    "../../../vendor/tree-sitter-asciidoc/tree-sitter-asciidoc_inline/queries/injections.scm"
);

/// Name under which the block queries inject the inline grammar.
const INLINE_LANGUAGE: &str = "asciidoc_inline";

/// The two highlight configurations and the legend entry of each highlight.
struct Highlighting {
    block: HighlightConfiguration,
    inline: HighlightConfiguration,
    /// Indexed like `STYLES`: token type index and modifier bitset.
    styles: Vec<(u32, u32)>,
}

impl Highlighting {
    fn load() -> anyhow::Result<Self> {
        let names: Vec<&str> = STYLES.iter().map(|(name, ..)| *name).collect();
        let mut block = HighlightConfiguration::new(
            asciidoc_core::parser::block_language(),
            "asciidoc",
            &without_spell_captures(BLOCK_HIGHLIGHTS),
            BLOCK_INJECTIONS,
            "",
        )?;
        let mut inline = HighlightConfiguration::new(
            asciidoc_core::parser::inline_language(),
            INLINE_LANGUAGE,
            &without_spell_captures(INLINE_HIGHLIGHTS),
            INLINE_INJECTIONS,
            "",
        )?;
        block.configure(&names);
        inline.configure(&names);

        let styles = STYLES
            .iter()
            .map(|(name, ty, modifiers)| {
                let ty = TOKEN_TYPES
                    .iter()
                    .position(|candidate| candidate == ty)
                    .ok_or_else(|| anyhow::anyhow!("{name}: type {ty} is not in the legend"))?;
                let mut bits = 0;
                for modifier in *modifiers {
                    let bit = TOKEN_MODIFIERS
                        .iter()
                        .position(|candidate| candidate == modifier)
                        .ok_or_else(|| {
                            anyhow::anyhow!("{name}: modifier {modifier} is not in the legend")
                        })?;
                    bits |= 1 << bit;
                }
                Ok((u32::try_from(ty)?, bits))
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self {
            block,
            inline,
            styles,
        })
    }
}

/// The queries are written for Neovim and tag text with `@spell` and
/// `@nospell`. `tree-sitter-highlight` lets the last capture of a node win,
/// so a node tagged `@comment @spell` would lose its comment style. The
/// server has no use for spell captures, so they are cut from the query text.
fn without_spell_captures(query: &str) -> String {
    query.replace("@nospell", "").replace("@spell", "")
}

/// Highlight the document: one token per styled run of text, as the LSP
/// delta encoding. Tokens never overlap and never span a line break.
///
/// Block and inline trees are highlighted as the grammar's injection queries
/// prescribe. Foreign languages (`[source,ruby]`, diagrams, math) have no
/// grammar here and stay unhighlighted: their text is a raw token.
///
/// # Errors
/// Returns an error if the highlight queries do not load or tree-sitter
/// fails to highlight the text.
pub fn semantic_tokens(content: &str) -> anyhow::Result<Vec<SemanticToken>> {
    static HIGHLIGHTING: OnceLock<Result<Highlighting, String>> = OnceLock::new();
    let highlighting = HIGHLIGHTING
        .get_or_init(|| Highlighting::load().map_err(|e| format!("{e:#}")))
        .as_ref()
        .map_err(|e| anyhow::anyhow!("semantic token queries: {e}"))?;

    let mut highlighter = Highlighter::new();
    let events = highlighter.highlight(
        &highlighting.block,
        content.as_bytes(),
        None,
        None,
        |language| (language == INLINE_LANGUAGE).then_some(&highlighting.inline),
    )?;

    // The events nest: the innermost enclosing highlight styles a run of
    // text, so tokens come out flat.
    let mut open: Vec<usize> = Vec::new();
    let mut tokens = TokenEncoder::new(content);
    for event in events {
        match event? {
            HighlightEvent::HighlightStart(Highlight(index)) => open.push(index),
            HighlightEvent::HighlightEnd => {
                open.pop();
            }
            HighlightEvent::Source { start, end } => {
                if let Some(&(token_type, modifiers)) = open
                    .last()
                    .and_then(|index| highlighting.styles.get(*index))
                {
                    tokens.push(start, end, token_type, modifiers);
                }
            }
        }
    }
    Ok(tokens.finish())
}

/// Turns byte ranges into delta-encoded tokens: splits at line breaks and
/// counts columns in UTF-16 code units.
struct TokenEncoder<'a> {
    content: &'a str,
    line_starts: Vec<usize>,
    tokens: Vec<SemanticToken>,
    previous_line: u32,
    previous_start: u32,
}

impl<'a> TokenEncoder<'a> {
    fn new(content: &'a str) -> Self {
        let line_starts = std::iter::once(0)
            .chain(content.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        Self {
            content,
            line_starts,
            tokens: Vec::new(),
            previous_line: 0,
            previous_start: 0,
        }
    }

    /// Add the bytes `start..end`, one token per line they touch. Ranges
    /// must come in document order.
    fn push(&mut self, mut start: usize, end: usize, token_type: u32, modifiers: u32) {
        while start < end {
            let line = self.line_starts.partition_point(|&s| s <= start) - 1;
            let line_start = self.line_starts[line];
            let next_line_start = self
                .line_starts
                .get(line + 1)
                .copied()
                .unwrap_or(self.content.len());
            let segment_end = end.min(next_line_start);
            let Some(segment) = self.content.get(start..segment_end) else {
                return;
            };
            let text = segment.trim_end_matches(['\n', '\r']);
            if !text.is_empty() {
                let Some(before) = self.content.get(line_start..start) else {
                    return;
                };
                self.add(
                    line,
                    utf16_len(before),
                    utf16_len(text),
                    token_type,
                    modifiers,
                );
            }
            start = segment_end;
        }
    }

    // LSP positions are u32; real-world documents stay far below that.
    #[allow(clippy::cast_possible_truncation)]
    fn add(&mut self, line: usize, column: u32, length: u32, token_type: u32, modifiers: u32) {
        let line = line as u32;
        let delta_start = if line == self.previous_line {
            column - self.previous_start
        } else {
            column
        };
        self.tokens.push(SemanticToken {
            delta_line: line - self.previous_line,
            delta_start,
            length,
            token_type,
            token_modifiers_bitset: modifiers,
        });
        self.previous_line = line;
        self.previous_start = column;
    }

    fn finish(self) -> Vec<SemanticToken> {
        self.tokens
    }
}

// LSP columns are u32; real-world lines stay far below that.
#[allow(clippy::cast_possible_truncation)]
fn utf16_len(text: &str) -> u32 {
    text.encode_utf16().count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../tests/fixtures/semantic_tokens/main.adoc");

    /// A decoded token: absolute position, covered text, type name and
    /// modifier names.
    #[derive(Debug, PartialEq, Eq)]
    struct Decoded {
        line: u32,
        start: u32,
        len: u32,
        text: String,
        ty: &'static str,
        mods: Vec<&'static str>,
    }

    fn decode(content: &str, tokens: &[SemanticToken]) -> Vec<Decoded> {
        let lines: Vec<&str> = content.split('\n').collect();
        let (mut line, mut start) = (0u32, 0u32);
        tokens
            .iter()
            .map(|t| {
                line += t.delta_line;
                start = if t.delta_line == 0 {
                    start + t.delta_start
                } else {
                    t.delta_start
                };
                let units: Vec<u16> = lines[line as usize].encode_utf16().collect();
                let text = String::from_utf16(&units[start as usize..(start + t.length) as usize])
                    .unwrap();
                let mods = TOKEN_MODIFIERS
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| t.token_modifiers_bitset & (1 << i) != 0)
                    .map(|(_, m)| *m)
                    .collect();
                Decoded {
                    line,
                    start,
                    len: t.length,
                    text,
                    ty: TOKEN_TYPES[t.token_type as usize],
                    mods,
                }
            })
            .collect()
    }

    fn decoded(content: &str) -> Vec<Decoded> {
        decode(content, &semantic_tokens(content).unwrap())
    }

    fn on_line(tokens: &[Decoded], line: u32) -> Vec<&Decoded> {
        tokens.iter().filter(|t| t.line == line).collect()
    }

    fn find<'a>(tokens: &'a [Decoded], ty: &str, text: &str) -> &'a Decoded {
        tokens
            .iter()
            .find(|t| t.ty == ty && t.text == text)
            .unwrap_or_else(|| panic!("no {ty} token {text:?} in {tokens:#?}"))
    }

    #[test]
    fn heading_carries_its_level() {
        let tokens = decoded(FIXTURE);

        let title = find(&tokens, "heading", "= Document Title");
        assert_eq!(title.mods, ["level1"]);
        let section = find(&tokens, "heading", "== Section One");
        assert_eq!(section.mods, ["level2"]);
    }

    #[test]
    fn bold_text_is_strong() {
        let tokens = decoded(FIXTURE);

        assert!(
            tokens
                .iter()
                .any(|t| t.ty == "strong" && t.line == 7 && t.text.contains("bold")),
            "{tokens:#?}"
        );
    }

    #[test]
    fn checklist_markers_carry_their_state() {
        let tokens = decoded("* [x] done\n* [ ] open\n* plain\n");

        let checked = find(&tokens, "listMarker", "[x]");
        assert_eq!(checked.mods, ["checked"]);
        let unchecked = find(&tokens, "listMarker", "[ ]");
        assert_eq!(unchecked.mods, ["unchecked"]);
        let plain = on_line(&tokens, 2);
        assert!(plain.iter().all(|t| t.mods.is_empty()), "{plain:#?}");
    }

    /// `configure` styles a capture only when `STYLES` names it, so a query
    /// capture without an entry silently loses its highlighting (and, as the
    /// last capture of a node wins, so does the node's other captures).
    /// Private (`_`) and injection captures are not highlights.
    #[test]
    fn every_query_capture_has_a_style() {
        let highlighting = Highlighting::load().unwrap();
        let known: Vec<&str> = STYLES.iter().map(|(name, ..)| *name).collect();
        for config in [&highlighting.block, &highlighting.inline] {
            for capture in config.names() {
                assert!(
                    capture.starts_with('_')
                        || capture.starts_with("injection.")
                        || known.contains(capture),
                    "capture @{capture} has no entry in STYLES"
                );
            }
        }
    }

    /// "xref", "link", "image" and "menu" in prose are words, not macros; the
    /// macro name of `btn:[OK]` stays a keyword. Asciidoctor renders the
    /// prose line as plain `<p>` text; `btn:[OK]` stays literal there, as
    /// the `experimental` attribute is unset.
    #[test]
    fn macro_names_are_keywords_only_in_a_macro() {
        let tokens = decoded(FIXTURE);

        let prose = on_line(&tokens, 33);
        assert!(prose.iter().all(|t| t.ty != "keyword"), "{prose:#?}");
        let macro_line = on_line(&tokens, 35);
        assert!(
            macro_line
                .iter()
                .any(|t| t.ty == "keyword" && t.text == "btn"),
            "{macro_line:#?}"
        );
    }

    /// A `....` literal block is verbatim like a listing: its body is raw and
    /// gets no inline markup. Asciidoctor: `<pre>literal *not bold*</pre>`.
    #[test]
    fn literal_block_body_is_raw() {
        let tokens = decoded(FIXTURE);

        let body = on_line(&tokens, 40);
        assert!(
            body.iter()
                .any(|t| t.ty == "raw" && t.text.contains("literal")),
            "{body:#?}"
        );
        assert!(body.iter().all(|t| t.ty != "strong"), "{body:#?}");
    }

    #[test]
    fn xref_target_is_a_link() {
        let tokens = decoded(FIXTURE);

        find(&tokens, "link", "_section_two");
    }

    #[test]
    fn line_comment_is_one_comment_token() {
        let tokens = decoded(FIXTURE);

        let on_comment_line = on_line(&tokens, 3);
        assert_eq!(on_comment_line.len(), 1, "{on_comment_line:#?}");
        assert_eq!(on_comment_line[0].ty, "comment");
        assert_eq!(on_comment_line[0].text, "// a line comment");
    }

    #[test]
    fn block_comment_gets_one_token_per_line() {
        let tokens = decoded(FIXTURE);

        for (line, text) in [
            (14, "////"),
            (15, "block comment"),
            (16, "spanning lines"),
            (17, "////"),
        ] {
            let on = on_line(&tokens, line);
            assert_eq!(on.len(), 1, "line {line}: {on:#?}");
            assert_eq!((on[0].ty, on[0].text.as_str()), ("comment", text));
        }
    }

    /// The body of a source block stays raw: no foreign grammar is loaded,
    /// so `fn` is not a keyword.
    #[test]
    fn source_block_body_is_raw_without_foreign_highlighting() {
        let tokens = decoded(FIXTURE);

        let body = on_line(&tokens, 21);
        assert_eq!(body.len(), 1, "{body:#?}");
        assert_eq!((body[0].ty, body[0].text.as_str()), ("raw", "fn main() {}"));
    }

    #[test]
    fn construct_over_a_line_break_is_split_per_line() {
        let tokens = decoded(FIXTURE);

        let first = on_line(&tokens, 9);
        let second = on_line(&tokens, 10);
        assert!(
            first
                .iter()
                .any(|t| t.ty == "strong" && t.text.contains("bold that")),
            "{first:#?}"
        );
        assert!(
            second
                .iter()
                .any(|t| t.ty == "strong" && t.text.contains("spans two lines")),
            "{second:#?}"
        );
    }

    #[test]
    fn admonition_label_is_marked_with_its_severity() {
        let tokens = decoded(FIXTURE);

        let note = tokens
            .iter()
            .find(|t| t.ty == "admonition" && t.line == 24)
            .unwrap();
        assert_eq!(note.mods, ["note"]);
    }

    #[test]
    fn columns_count_utf16_code_units() {
        let tokens = decoded(FIXTURE);

        // "Umlaut Ünï 😀 " is 14 UTF-16 units; the emoji takes two.
        let bold = tokens
            .iter()
            .find(|t| t.ty == "strong" && t.line == 12)
            .unwrap();
        assert_eq!(bold.start, 14, "{bold:?}");
        assert_eq!(
            bold.len,
            u32::try_from(bold.text.encode_utf16().count()).unwrap()
        );
    }

    /// Two captures on one node (`@comment @spell`, `@markup.link.url
    /// @markup.link`) give one token, not two stacked ones.
    #[test]
    fn two_captures_on_one_node_give_one_token() {
        let tokens = decoded(FIXTURE);

        let url: Vec<_> = tokens
            .iter()
            .filter(|t| t.text.contains("example.com"))
            .collect();
        assert_eq!(url.len(), 1, "{url:#?}");
        assert_eq!(url[0].ty, "link");
    }

    #[test]
    fn tokens_are_ordered_non_empty_and_never_overlap() {
        for content in [FIXTURE.to_string(), FIXTURE.replace('\n', "\r\n")] {
            let tokens = decoded(&content);
            assert!(tokens.len() > 20, "{} tokens", tokens.len());

            let mut previous_end = (0u32, 0u32);
            for t in &tokens {
                assert!(t.len > 0, "empty token {t:?}");
                assert!(!t.text.contains(['\n', '\r']), "multi-line token {t:?}");
                assert!(
                    (t.line, t.start) >= previous_end,
                    "{t:?} starts before {previous_end:?}"
                );
                previous_end = (t.line, t.start + t.len);
            }
        }
    }

    #[test]
    fn empty_document_has_no_tokens() {
        assert!(semantic_tokens("").unwrap().is_empty());
    }
}
