//! Ventilated prose: rewrite a paragraph to one sentence per line.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use crate::error::CoreError;

/// Replacement for one paragraph: byte range in the document and its new text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VentilateEdit {
    pub range: Range<usize>,
    pub new_text: String,
}

/// Rewrite the paragraph at `line` (1-indexed) to one sentence per line.
///
/// Returns `Ok(None)` when the line is not in a paragraph or the paragraph
/// is already ventilated.
///
/// # Errors
/// Returns an error if the parser cannot be initialized or fails to parse.
pub fn ventilate_paragraph(source: &str, line: usize) -> Result<Option<VentilateEdit>, CoreError> {
    let mut parser = crate::parser::init_block_parser()?;
    let tree = crate::parser::parse(&mut parser, source)?;
    let lines = line_spans(source);

    let layout = layout(tree.root_node(), source);
    let Some(group) = group_at(&layout.paragraphs, line.saturating_sub(1)) else {
        return Ok(None);
    };
    let mut error_rows = Vec::new();
    walk_for_error_rows(tree.root_node(), &mut error_rows);
    // An error right above or below may be the rest of this paragraph.
    let neighbourhood = group.start.saturating_sub(1)..group.end + 1;
    let group_lines = &lines[group.clone()];
    let contents: Vec<&str> = group_lines
        .iter()
        .map(|span| &source[span.content.clone()])
        .collect();

    if layout.continues_item(group.start)
        || layout.ruled_out_above(group.start)
        // A block line right below a paragraph line is text of that paragraph.
        || layout.is_block_line(group.end)
        || crate::extractors::extract_attributes(tree.root_node(), source.as_bytes())
            .iter()
            .any(|attr| attr.name == "hardbreaks-option")
        // A line ending in ` +` is a hard line break.
        || contents.iter().any(|content| content.trim_end().ends_with(" +"))
        || error_rows
            .iter()
            .any(|rows| rows.start < neighbourhood.end && neighbourhood.start < rows.end)
        || inline_parse_fails(&contents)?
    {
        return Ok(None);
    }
    let range = group_lines[0].content.start..group_lines[group_lines.len() - 1].content.end;
    // The last line of a document may lack a line ending; take the group's
    // first one, else the document's.
    let eol = group_lines
        .iter()
        .chain(&lines)
        .map(|span| span.eol)
        .find(|eol| !eol.is_empty())
        .unwrap_or("\n");

    let joined = contents
        .iter()
        .map(|content| content.trim())
        .collect::<Vec<_>>()
        .join(" ");
    let starts = sentence_starts(&joined);
    let new_text = render(&joined, &starts, eol);

    if new_text.is_empty() || new_text == source[range.clone()] {
        return Ok(None);
    }
    Ok(Some(VentilateEdit { range, new_text }))
}

/// Candidate sentence starts in `text` (byte offsets, ascending, never 0),
/// as UAX #29 sentence boundaries.
fn sentence_starts(text: &str) -> Vec<usize> {
    text.split_sentence_bound_indices()
        .map(|(start, _)| start)
        .filter(|&start| start > 0)
        .collect()
}

/// Split `text` at `starts` and write one sentence per line.
fn render(text: &str, starts: &[usize], eol: &str) -> String {
    let ends = starts.iter().copied().chain([text.len()]);
    let mut previous = 0;
    let mut sentences = Vec::new();
    for end in ends {
        let sentence = text[previous..end].trim_end();
        if !sentence.is_empty() {
            sentences.push(sentence);
        }
        previous = end;
    }
    sentences.join(eol)
}

/// One source line: its content bytes and the line ending that follows.
struct LineSpan<'a> {
    content: Range<usize>,
    eol: &'a str,
}

fn line_spans(source: &str) -> Vec<LineSpan<'_>> {
    let mut spans = Vec::new();
    let mut start = 0;
    for raw in source.split_inclusive('\n') {
        let content_len = raw.trim_end_matches(['\n', '\r']).len();
        let content = start..start + content_len;
        spans.push(LineSpan {
            content,
            eol: &raw[content_len..],
        });
        start += raw.len();
    }
    spans
}

/// What the block tree says about the rows of the document.
#[derive(Default)]
struct Layout<'a> {
    /// Rows (0-indexed) of paragraph lines at the top level or directly
    /// inside sidebar and example blocks, in document order.
    paragraphs: Vec<usize>,
    /// Rows covered by list items and admonitions. Paragraph lines right
    /// after them continue the item rather than start a paragraph.
    items: Vec<Range<usize>>,
    /// Block attribute lines: row and attribute list (without brackets).
    attributes: Vec<(usize, &'a str)>,
    /// Block title lines.
    titles: Vec<usize>,
}

impl<'a> Layout<'a> {
    fn continues_item(&self, row: usize) -> bool {
        row > 0 && self.items.iter().any(|rows| rows.contains(&(row - 1)))
    }

    /// Attribute lists of the block attribute lines directly above `row`,
    /// looking past block titles. `None` if those lines follow a paragraph
    /// line: Asciidoctor then reads them as text of that paragraph.
    fn attributes_above(&self, row: usize) -> Option<Vec<&'a str>> {
        let mut attributes = Vec::new();
        let mut above = row;
        while above > 0 && self.is_block_line(above - 1) {
            above -= 1;
            if let Some(&(_, attr)) = self.attributes.iter().find(|(r, _)| *r == above) {
                attributes.push(attr);
            }
        }
        let follows_paragraph = above < row && above > 0 && self.paragraphs.contains(&(above - 1));
        (!follows_paragraph).then_some(attributes)
    }

    /// Whether `row` is a block attribute or block title line.
    fn is_block_line(&self, row: usize) -> bool {
        self.titles.contains(&row) || self.attributes.iter().any(|(r, _)| *r == row)
    }

    /// Whether the block attributes above `row` rule ventilation out, or
    /// cannot be told apart from paragraph text.
    fn ruled_out_above(&self, row: usize) -> bool {
        self.attributes_above(row)
            .is_none_or(|attributes| attributes.into_iter().any(rules_out_ventilation))
    }
}

fn layout<'a>(root: tree_sitter::Node, source: &'a str) -> Layout<'a> {
    let mut layout = Layout::default();
    walk_for_layout(root, source, &mut layout);
    layout
}

fn walk_for_layout<'a>(node: tree_sitter::Node, source: &'a str, layout: &mut Layout<'a>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let row = child.start_position().row;
        match child.kind() {
            "paragraph" if node.kind() == "section_block" => {
                // The grammar reads `[[id]]` and `[#id]` lines as paragraph
                // text; Asciidoctor reads them as block attribute lines.
                match block_attribute_line(source[child.byte_range()].trim()) {
                    Some(attr) => layout.attributes.push((row, attr)),
                    None => layout.paragraphs.push(row),
                }
            }
            "element_attr" => {
                let attr = child
                    .children(&mut child.walk())
                    .find(|n| n.kind() == "attr_value")
                    .map_or("", |n| &source[n.byte_range()]);
                layout.attributes.push((row, attr));
            }
            "block_title" => layout.titles.push(row),
            "list" | "admonition" => layout.items.push(rows_of(child)),
            "block_element" | "section_block" => walk_for_layout(child, source, layout),
            // A styled delimiter (`[NOTE]` over `====`) makes it another block.
            "sidebar_block" | "delimited_block" if !layout.ruled_out_above(row) => {
                walk_for_layout(child, source, layout);
            }
            _ => {}
        }
    }
}

/// Rows covered by `ERROR` and `MISSING` nodes.
fn walk_for_error_rows(node: tree_sitter::Node, rows: &mut Vec<Range<usize>>) {
    if node.is_error() || node.is_missing() {
        let covered = rows_of(node);
        rows.push(covered.start..covered.end.max(covered.start + 1));
        return;
    }
    if !node.has_error() {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_error_rows(child, rows);
    }
}

/// Whether the inline grammar fails on any of `lines`.
fn inline_parse_fails(lines: &[&str]) -> Result<bool, CoreError> {
    let mut parser = crate::parser::init_inline_parser()?;
    for line in lines {
        if crate::parser::parse(&mut parser, line)?
            .root_node()
            .has_error()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The attribute list of a block attribute line, or `None` for other lines.
/// A block anchor (`[[id]]`) carries no style or options.
fn block_attribute_line(line: &str) -> Option<&str> {
    if line.starts_with("[[") && line.ends_with("]]") {
        return Some("");
    }
    line.strip_prefix('[')?.strip_suffix(']')
}

/// Whether a block attribute list rules ventilation out: a style or option
/// under which line breaks are significant, or an admonition or quote style
/// the MVP leaves alone.
fn rules_out_ventilation(attr: &str) -> bool {
    let entries = split_attribute_list(attr);
    let positional_options = entries
        .first()
        .filter(|first| !first.contains('='))
        .is_some_and(|first| {
            let style_end = first.find(['#', '.', '%']).unwrap_or(first.len());
            let (style, shorthands) = first.split_at(style_end);
            matches!(
                style.trim(),
                "verse"
                    | "source"
                    | "literal"
                    | "pass"
                    | "listing"
                    | "quote"
                    | "NOTE"
                    | "TIP"
                    | "IMPORTANT"
                    | "WARNING"
                    | "CAUTION"
            ) || shorthands
                .split('%')
                .skip(1)
                .any(|option| option.split(['#', '.']).next() == Some("hardbreaks"))
        });
    let named_options = entries
        .iter()
        .filter_map(|entry| entry.split_once('='))
        .any(|(name, value)| {
            matches!(name.trim(), "options" | "opts")
                && value
                    .trim()
                    .trim_matches('"')
                    .split(',')
                    .any(|option| option.trim() == "hardbreaks")
        });
    positional_options || named_options
}

/// Split an attribute list at commas outside double quotes.
fn split_attribute_list(attr: &str) -> Vec<&str> {
    let mut entries = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    for (i, c) in attr.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                entries.push(&attr[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    entries.push(&attr[start..]);
    entries
}

/// Rows a node covers; a node ending at column 0 does not cover that row.
fn rows_of(node: tree_sitter::Node) -> Range<usize> {
    let end = node.end_position();
    let last = if end.column == 0 {
        end.row
    } else {
        end.row + 1
    };
    node.start_position().row..last
}

/// The run of consecutive rows that contains `row`, as a row range.
fn group_at(rows: &[usize], row: usize) -> Option<Range<usize>> {
    let idx = rows.iter().position(|&r| r == row)?;
    let mut start = idx;
    while start > 0 && rows[start - 1] + 1 == rows[start] {
        start -= 1;
    }
    let mut end = idx;
    while end + 1 < rows.len() && rows[end] + 1 == rows[end + 1] {
        end += 1;
    }
    Some(rows[start]..rows[end] + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    /// Apply the edit for the paragraph at `line` and return the new document.
    fn apply(source: &str, line: usize) -> String {
        match ventilate_paragraph(source, line).unwrap() {
            Some(edit) => {
                let mut out = source.to_string();
                out.replace_range(edit.range, &edit.new_text);
                out
            }
            None => source.to_string(),
        }
    }

    fn collapse_whitespace(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[rstest]
    // Test-spec 1.1
    #[case::two_sentences_on_one_line(
        "First sentence. Second sentence.\n",
        1,
        "First sentence.\nSecond sentence.\n"
    )]
    // Test-spec 1.2
    #[case::wrapped_sentence_is_joined(
        "This sentence was wrapped\nat seventy-two columns by an editor.\n",
        1,
        "This sentence was wrapped at seventy-two columns by an editor.\n"
    )]
    // Test-spec 1.3
    #[case::wrapped_and_multiple_sentences(
        "One. Two is\nwrapped. Three.\n",
        1,
        "One.\nTwo is wrapped.\nThree.\n"
    )]
    // Test-spec 1.4
    #[case::already_ventilated("One.\nTwo.\nThree.\n", 2, "One.\nTwo.\nThree.\n")]
    // Test-spec 1.5
    #[case::single_sentence("Just one sentence.\n", 1, "Just one sentence.\n")]
    // Test-spec 1.6
    #[case::no_terminal_punctuation(
        "No period at the end\nof this paragraph\n",
        1,
        "No period at the end of this paragraph\n"
    )]
    #[case::long_sentence_stays_on_one_line(
        "This is a rather long sentence that goes on and on well past any usual column limit without ever ending in a way that would invite wrapping.\n",
        1,
        "This is a rather long sentence that goes on and on well past any usual column limit without ever ending in a way that would invite wrapping.\n"
    )]
    // Test-spec 2.1
    #[case::question_and_exclamation("Is it? Yes! Done.\n", 1, "Is it?\nYes!\nDone.\n")]
    // Test-spec 2.4
    #[case::colon_and_semicolon_do_not_split(
        "Two options: fast; slow. End.\n",
        1,
        "Two options: fast; slow.\nEnd.\n"
    )]
    // Test-spec 2.5
    #[case::multiple_spaces_after_period("First.   Second.\n", 1, "First.\nSecond.\n")]
    // Test-spec 8.1
    #[case::trailing_whitespace_on_wrapped_line(
        "Wrapped here   \nand joined.\n",
        1,
        "Wrapped here and joined.\n"
    )]
    // Test-spec 8.2
    #[case::whitespace_inside_line_is_kept(
        "Tab\there and  two spaces.\nNext\tline.\n",
        1,
        "Tab\there and  two spaces.\nNext\tline.\n"
    )]
    // Test-spec 8.3
    #[case::crlf_stays_crlf(
        "One. Two is\r\nwrapped. Three.\r\n",
        1,
        "One.\r\nTwo is wrapped.\r\nThree.\r\n"
    )]
    // Test-spec 8.4
    #[case::umlauts(
        "Größe ändern. Übernächster Satz.\n",
        1,
        "Größe ändern.\nÜbernächster Satz.\n"
    )]
    // Test-spec 8.6
    #[case::emoji_before_split("Party 🎉 time. Next.\n", 1, "Party 🎉 time.\nNext.\n")]
    #[case::cursor_on_last_line_formats_whole_paragraph(
        "One. Two is\nwrapped. Three.\n",
        2,
        "One.\nTwo is wrapped.\nThree.\n"
    )]
    #[case::only_the_paragraph_under_cursor(
        "First. Second.\n\nThird. Fourth.\n",
        3,
        "First. Second.\n\nThird.\nFourth.\n"
    )]
    #[case::paragraph_after_title("= Title\n\nFirst. Second.\n", 3, "= Title\n\nFirst.\nSecond.\n")]
    #[case::paragraph_under_section_title(
        "= Doc\n\n== Section\n\nFirst. Second.\n",
        5,
        "= Doc\n\n== Section\n\nFirst.\nSecond.\n"
    )]
    #[case::paragraph_without_final_newline("First. Second.", 1, "First.\nSecond.")]
    #[case::crlf_paragraph_without_final_newline("A.\r\n\r\nB. C.", 3, "A.\r\n\r\nB.\r\nC.")]
    // Test-spec 7.2
    #[case::paragraph_in_sidebar(
        "****\nSide one. Is\nwrapped. Two.\n****\n",
        3,
        "****\nSide one.\nIs wrapped.\nTwo.\n****\n"
    )]
    #[case::paragraph_in_titled_example_block(
        ".Example\n[#ex1]\n====\nOne. Two.\n====\n",
        4,
        ".Example\n[#ex1]\n====\nOne.\nTwo.\n====\n"
    )]
    #[case::paragraph_in_example_block(
        "====\nExample one. Two.\n====\n",
        2,
        "====\nExample one.\nTwo.\n====\n"
    )]
    #[case::paragraph_after_list_and_blank_line(
        "* Item.\n\nAfter one. Two.\n",
        3,
        "* Item.\n\nAfter one.\nTwo.\n"
    )]
    #[case::paragraph_after_admonition_and_blank_line(
        "NOTE: Note.\n\nAfter one. Two.\n",
        3,
        "NOTE: Note.\n\nAfter one.\nTwo.\n"
    )]
    #[case::id_and_role_attribute(
        "[#intro.lead]\nFirst. Second.\n",
        2,
        "[#intro.lead]\nFirst.\nSecond.\n"
    )]
    #[case::block_title(".Title\nFirst. Second.\n", 2, ".Title\nFirst.\nSecond.\n")]
    #[case::plus_inside_line_is_no_hard_break("One + two. Three.\n", 1, "One + two.\nThree.\n")]
    #[case::block_parse_error_after_blank_line(
        "One. Two.\n\n[unclosed\n",
        1,
        "One.\nTwo.\n\n[unclosed\n"
    )]
    #[case::block_anchor("[[sec-a]]\nFirst. Second.\n", 2, "[[sec-a]]\nFirst.\nSecond.\n")]
    #[case::block_id("[#intro]\nFirst. Second.\n", 2, "[#intro]\nFirst.\nSecond.\n")]
    #[case::block_id_after_paragraph_and_blank_line(
        "Intro.\n\n[#intro]\nFirst. Second.\n",
        4,
        "Intro.\n\n[#intro]\nFirst.\nSecond.\n"
    )]
    fn ventilates_paragraph_under_cursor(
        #[case] source: &str,
        #[case] line: usize,
        #[case] expected: &str,
    ) {
        let once = apply(source, line);
        assert_eq!(once, expected);
        assert_eq!(apply(&once, line), once, "not idempotent");
        assert_eq!(
            collapse_whitespace(&once),
            collapse_whitespace(source),
            "text changed beyond whitespace"
        );
    }

    #[rstest]
    // Test-spec 8.7
    #[case::empty_document("", 1)]
    #[case::whitespace_only_document("  \n\t\n", 1)]
    #[case::cursor_on_blank_line("First. Second.\n\nThird.\n", 2)]
    #[case::cursor_on_section_title("== Title. With dot\n\nFirst. Second.\n", 1)]
    #[case::cursor_past_end("First. Second.\n", 5)]
    #[case::already_ventilated("One.\nTwo.\n", 1)]
    // Test-spec 7.4
    #[case::list_item("* Item one. Two.\n", 1)]
    #[case::list_item_continuation("* Item one. Two.\ncontinued here. More.\n", 2)]
    #[case::ordered_list_item_continuation(". Item one. Two.\ncontinued here. More.\n", 2)]
    #[case::description_list_item_continuation("CPU:: Def one. Two.\nmore def. Yes.\n", 2)]
    #[case::block_attached_to_list_item("* Item.\n+\nAttached one. Two.\n", 3)]
    #[case::admonition("NOTE: First. Second.\n", 1)]
    #[case::admonition_continuation("NOTE: First. Second.\ncontinued. Yes.\n", 2)]
    // Test-spec 6.9: the block attribute applies to every line of the group
    #[case::verse_style_on_first_line("[verse]\nLine one. Two.\nLine three.\n", 2)]
    #[case::verse_style_on_later_line("[verse]\nLine one. Two.\nLine three.\n", 3)]
    #[case::source_style("[source]\nlet a = 1. Two.\n", 2)]
    #[case::source_style_with_language("[source,rust]\nlet a = 1. Two.\n", 2)]
    #[case::literal_style("[literal]\nOne. Two.\n", 2)]
    #[case::pass_style("[pass]\nOne. Two.\n", 2)]
    #[case::listing_style("[listing]\nOne. Two.\n", 2)]
    #[case::verse_with_author("[verse,Carl Sandburg,Fog]\nOne. Two.\n", 2)]
    #[case::verse_below_block_title(".Fog\n[verse]\nOne. Two.\n", 3)]
    // Story 4: a block attribute or title line inside a paragraph is text
    #[case::attribute_line_inside_paragraph_above("One. Two.\n[x]\nThree. Four.\n", 1)]
    #[case::attribute_line_inside_paragraph_below("One. Two.\n[x]\nThree. Four.\n", 3)]
    #[case::anchor_line_inside_paragraph("One. Two.\n[[id]]\nThree. Four.\n", 3)]
    #[case::title_line_inside_paragraph("One. Two.\n.Title\nThree. Four.\n", 1)]
    // Stories 39, 40: admonition and quote styles
    #[case::note_style("[NOTE]\nOne. Two.\n", 2)]
    #[case::caution_style("[CAUTION]\nOne. Two.\n", 2)]
    #[case::quote_style("[quote]\nOne. Two.\n", 2)]
    #[case::admonition_block("[NOTE]\n====\nOne. Two.\n====\n", 3)]
    #[case::verse_styled_sidebar("[verse]\n****\nOne. Two.\n****\n", 3)]
    #[case::quote_style_with_attribution("[quote,Author,Source]\nOne. Two.\n", 2)]
    // Test-spec 6.1
    #[case::hardbreaks_option("[%hardbreaks]\nOne. Two.\nThree.\n", 3)]
    #[case::hardbreaks_option_after_style("[normal%hardbreaks]\nOne. Two.\n", 2)]
    #[case::hardbreaks_named_options("[options=hardbreaks]\nOne. Two.\n", 2)]
    #[case::hardbreaks_quoted_opts("[opts=\"autofit,hardbreaks\"]\nOne. Two.\n", 2)]
    #[case::hardbreaks_after_id("[#intro%hardbreaks]\nOne. Two.\n", 2)]
    #[case::cursor_on_block_anchor("[[sec-a]]\nFirst. Second.\n", 1)]
    // Test-spec 6.3–6.8, 7.3: the grammar gives these no paragraph node
    #[case::literal_paragraph(" Literal. Text.\n", 1)]
    #[case::listing_block("----\nList. Ing.\n----\n", 2)]
    #[case::literal_block("....\nLit. Eral.\n....\n", 2)]
    #[case::passthrough_block("++++\nPass. Through.\n++++\n", 2)]
    #[case::table_cell("|===\n| A. B. | C\n|===\n", 2)]
    #[case::line_comment("// Comment. Here.\n", 1)]
    #[case::comment_block("////\nComment. Block.\n////\n", 2)]
    #[case::attribute_entry(":attr: Val. Ue.\n", 1)]
    #[case::block_attribute_line("[source,rust]\nlet a = 1;\n", 1)]
    #[case::block_title_line(".Title. With dot\nFirst. Second.\n", 1)]
    #[case::quote_block("____\nQuote. Two.\n____\n", 2)]
    // Story 42
    #[case::block_parse_error_right_below("One. Two.\n[unclosed\nThree. Four.\n", 1)]
    #[case::bold_spanning_two_lines("Some *bold\ntext* here. Two.\n", 1)]
    #[case::description_list_with_lowercase_term("term:: Def. Two.\nmore. Yes.\n", 2)]
    // Test-spec 6.2
    #[case::hard_line_break("Hard. Break +\nnext. Line.\n", 1)]
    #[case::hard_line_break_on_later_line("First. Line.\nHard. Break +\nnext.\n", 1)]
    #[case::hard_line_break_with_trailing_whitespace("Hard. Break +  \nnext. Line.\n", 1)]
    #[case::hard_line_break_crlf("Hard. Break +\r\nnext. Line.\r\n", 2)]
    #[case::hardbreaks_document_attribute(":hardbreaks-option:\n\nOne. Two.\nThree.\n", 3)]
    #[case::hardbreaks_document_attribute_below_title(
        "= Doc\n:hardbreaks-option:\n\nOne. Two.\n",
        4
    )]
    fn offers_no_edit(#[case] source: &str, #[case] line: usize) {
        assert_eq!(ventilate_paragraph(source, line).unwrap(), None);
    }
}
