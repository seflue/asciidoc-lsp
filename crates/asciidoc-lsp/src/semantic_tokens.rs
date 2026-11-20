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
        // Inline highlights are numbered after the block ones, so an event
        // tells which tree its capture comes from. No capture matches "".
        let inline_names: Vec<&str> = std::iter::repeat_n("", STYLES.len())
            .chain(names.iter().copied())
            .collect();
        inline.configure(&inline_names);

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
    // text, so tokens come out flat. Where a block capture starts at the
    // same byte as an inline capture in the text it injects (the title line
    // of `= {product} Manual`), `tree-sitter-highlight` starts the inline
    // one first, though the block capture encloses it. A run of starts at
    // one byte is therefore stacked block before inline, so each end closes
    // the highlight it belongs to.
    let from_inline = |index: usize| index >= STYLES.len();
    let mut open: Vec<usize> = Vec::new();
    let mut run_start = 0;
    let mut tokens = TokenEncoder::new(content);
    for event in events {
        match event? {
            HighlightEvent::HighlightStart(Highlight(index)) => {
                let at = if from_inline(index) {
                    open.len()
                } else {
                    open[run_start..]
                        .iter()
                        .position(|&other| from_inline(other))
                        .map_or(open.len(), |offset| run_start + offset)
                };
                open.insert(at, index);
            }
            HighlightEvent::HighlightEnd => {
                open.pop();
                run_start = open.len();
            }
            HighlightEvent::Source { start, end } => {
                run_start = open.len();
                if let Some(&(token_type, modifiers)) = open
                    .last()
                    .and_then(|index| highlighting.styles.get(index % STYLES.len()))
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

    /// The document title is two heading tokens, marker and text, as the
    /// queries style those two parts of the header only.
    #[test]
    fn heading_carries_its_level() {
        let tokens = decoded(FIXTURE);

        for text in ["=", "Document Title"] {
            let title = find(&tokens, "heading", text);
            assert_eq!(title.mods, ["level1"]);
        }
        let section = find(&tokens, "heading", "== Section One");
        assert_eq!(section.mods, ["level2"]);
    }

    /// Only the title line of the header is a heading: the author and
    /// revision lines, gaps included, carry no heading token. Asciidoctor
    /// reads the lines as author (`Ada Lovelace`), email
    /// (`ada@example.org`), revnumber (`1.0`), revdate (`2026-01-01`) and
    /// revremark (`Draft`).
    #[test]
    fn author_and_revision_lines_are_no_heading() {
        let tokens = decoded("= Title\nAda Lovelace <ada@example.org>\nv1.0, 2026-01-01: Draft\n");

        let title = on_line(&tokens, 0);
        assert!(
            !title.is_empty() && title.iter().all(|t| t.ty == "heading"),
            "{title:#?}"
        );
        for line in [1, 2] {
            let on = on_line(&tokens, line);
            assert!(on.iter().all(|t| t.ty != "heading"), "{on:#?}");
        }
    }

    /// A title that opens with an attribute reference keeps its heading:
    /// the reference is styled inside it and the rest of the title stays a
    /// heading. Asciidoctor resolves the reference in the title: `skipping
    /// reference to missing attribute: product`.
    #[test]
    fn title_opening_with_attribute_reference_stays_heading() {
        let tokens = decoded(include_str!(
            "../../../tests/fixtures/adversarial_project/index.adoc"
        ));

        assert_eq!(
            texts_on_line(&tokens, 0),
            [
                ("heading", "="),
                ("punctuation", "{"),
                ("variable", "product"),
                ("punctuation", "}"),
                ("heading", " User Manual"),
            ]
        );
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
    fn inline_markup_in_a_quote_block_paragraph_is_highlighted() {
        let tokens = decoded("____\nSome *bold* text and <<_target>>.\n____\n");

        find(&tokens, "strong", "*bold*");
        find(&tokens, "link", "_target");
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

    /// A listing without a style is raw too, and `*x*` in it is no bold.
    #[test]
    fn unstyled_listing_body_is_raw() {
        let tokens = decoded("----\n*x*\n----\n");

        let body = on_line(&tokens, 1);
        assert_eq!(body.len(), 1, "{body:#?}");
        assert_eq!((body[0].ty, body[0].text.as_str()), ("raw", "*x*"));
    }

    /// `source` of `[source,rust]` is the block style, a type.
    #[test]
    fn source_style_is_a_type_not_a_parameter() {
        let tokens = decoded("[source,rust]\n----\nfn x() {}\n----\n");

        let style = on_line(&tokens, 0)
            .into_iter()
            .find(|t| t.text == "source")
            .unwrap();
        assert_eq!(style.ty, "type");
    }

    /// Delimiters and text of a `[NOTE]` block carry the admonition token.
    #[test]
    fn admonition_block_body_and_delimiters_are_marked() {
        let tokens = decoded("[NOTE]\n====\ntext\n====\n");

        for (line, text) in [(1, "===="), (2, "text"), (3, "====")] {
            let on = on_line(&tokens, line);
            assert!(
                on.iter()
                    .any(|t| t.ty == "admonition" && t.text == text && t.mods == ["note"]),
                "{on:#?}"
            );
        }
    }

    /// A role after the admonition style keeps delimiters and body marked,
    /// and a multi-line, multi-paragraph body is marked on every line.
    #[test]
    fn admonition_with_role_and_long_body_is_marked() {
        let tokens = decoded("[NOTE.role]\n====\na1\na2\n\nb1\nb2\n====\n");

        for (line, text) in [
            (1, "===="),
            (2, "a1"),
            (3, "a2"),
            (5, "b1"),
            (6, "b2"),
            (7, "===="),
        ] {
            let on = on_line(&tokens, line);
            assert!(
                on.iter()
                    .any(|t| t.ty == "admonition" && t.text == text && t.mods == ["note"]),
                "line {line}: {on:#?}"
            );
        }
    }

    fn has_admonition(tokens: &[Decoded], line: u32, text: &str) -> bool {
        on_line(tokens, line)
            .iter()
            .any(|t| t.ty == "admonition" && t.text == text && t.mods == ["note"])
    }

    /// Every direct paragraph of a `[NOTE]` example block is marked, also
    /// those after a list or a listing, not only the leading run.
    #[test]
    fn admonition_block_marks_paragraphs_after_a_list() {
        let tokens =
            decoded("[NOTE]\n====\nfirst\n\n* item\n\nafter\n\n----\ncode\n----\n\nlast\n====\n");

        for (line, text) in [(2, "first"), (6, "after"), (12, "last")] {
            assert!(
                has_admonition(&tokens, line, text),
                "line {line}: {tokens:#?}"
            );
        }
    }

    /// The body of a `[NOTE]` open block is marked like an example block's.
    #[test]
    fn admonition_open_block_body_is_marked() {
        let tokens = decoded("[NOTE]\n--\nopen\n\n* item\n\nafter\n--\n");

        for (line, text) in [(2, "open"), (6, "after")] {
            assert!(
                has_admonition(&tokens, line, text),
                "line {line}: {tokens:#?}"
            );
        }
    }

    /// The style line is marked above a paragraph, and carries no admonition
    /// token above a listing, a sidebar or a list: the block is no
    /// admonition there, so neither its body.
    #[test]
    fn admonition_style_line_is_marked_only_above_admonition_blocks() {
        let paragraph = decoded("[NOTE]\ntext\n");
        assert!(has_admonition(&paragraph, 0, "NOTE"), "{paragraph:#?}");

        for content in [
            "[NOTE]\n----\ncode\n----\n",
            "[NOTE]\n****\nside\n****\n",
            "[NOTE]\n* item\n",
        ] {
            let tokens = decoded(content);
            assert!(
                tokens.iter().all(|t| t.ty != "admonition"),
                "{content:?}: {tokens:#?}"
            );
        }
    }

    /// A listing with named attributes only has no style: raw body.
    #[test]
    fn listing_with_named_attributes_only_is_raw() {
        let tokens = decoded("[subs=+quotes]\n----\n*x*\n----\n");

        let body = on_line(&tokens, 2);
        assert_eq!(body.len(), 1, "{body:#?}");
        assert_eq!((body[0].ty, body[0].text.as_str()), ("raw", "*x*"));
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

    /// Type and text of the tokens on one line.
    fn texts_on_line(tokens: &[Decoded], line: u32) -> Vec<(&str, &str)> {
        on_line(tokens, line)
            .iter()
            .map(|t| (t.ty, t.text.as_str()))
            .collect()
    }

    /// The role list of `[.term]#widget#` closes at its own `]`, not at the
    /// bracket of a later macro on the line. Asciidoctor: `Role text <span
    /// class="term">widget</span> comes before a later btn:[OK] macro.`
    #[test]
    fn role_list_closes_at_its_own_bracket() {
        let tokens = decoded(FIXTURE);

        let line = texts_on_line(&tokens, 35);
        assert_eq!(
            line[..4],
            [
                ("punctuation", "["),
                ("parameter", "term"),
                ("punctuation", "]"),
                ("emphasis", "#widget#"),
            ],
            "{line:#?}"
        );
    }

    /// A line that starts with `[` and ends with `]` is a block attribute
    /// list, role span or not, so it holds no styled text. Asciidoctor reads
    /// the whole line as the style of the next block: `unknown style for
    /// paragraph: .role]#styled# and later pass:q[x`.
    #[test]
    fn bracketed_line_is_an_attribute_list_not_role_text() {
        let tokens = decoded("[.role]#styled# and later pass:q[x]\n");

        assert!(tokens.iter().all(|t| t.ty != "emphasis"), "{tokens:#?}");
    }

    /// The angle brackets of `<https://example.org>` are markup around the
    /// URL, not part of it. Asciidoctor drops them: `The page <a
    /// href="https://example.org" class="bare">https://example.org</a> sits
    /// in angle brackets.`
    #[test]
    fn angle_brackets_are_not_part_of_the_link() {
        let tokens = decoded(FIXTURE);

        assert_eq!(
            texts_on_line(&tokens, 37),
            [
                ("punctuation", "<"),
                ("link", "https://example.org"),
                ("punctuation", ">"),
            ]
        );
    }

    #[test]
    fn empty_document_has_no_tokens() {
        assert!(semantic_tokens("").unwrap().is_empty());
    }
}
