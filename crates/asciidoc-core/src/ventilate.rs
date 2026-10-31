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

    let paragraph_rows = paragraph_rows(tree.root_node());
    let Some(group) = group_at(&paragraph_rows, line.saturating_sub(1)) else {
        return Ok(None);
    };

    let group_lines = &lines[group];
    let range = group_lines[0].content.start..group_lines[group_lines.len() - 1].content.end;
    // The last line of a document may lack a line ending; take the group's
    // first one, else the document's.
    let eol = group_lines
        .iter()
        .chain(&lines)
        .map(|span| span.eol)
        .find(|eol| !eol.is_empty())
        .unwrap_or("\n");

    let joined = group_lines
        .iter()
        .map(|span| source[span.content.clone()].trim())
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

/// Rows (0-indexed) of top-level paragraph lines, in document order.
fn paragraph_rows(root: tree_sitter::Node) -> Vec<usize> {
    let mut rows = Vec::new();
    let mut cursor = root.walk();
    for block in root.children(&mut cursor) {
        let mut block_cursor = block.walk();
        for section in block.children(&mut block_cursor) {
            if section.kind() != "section_block" {
                continue;
            }
            let mut section_cursor = section.walk();
            for child in section.children(&mut section_cursor) {
                if child.kind() == "paragraph" {
                    rows.push(child.start_position().row);
                }
            }
        }
    }
    rows
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
    fn offers_no_edit(#[case] source: &str, #[case] line: usize) {
        assert_eq!(ventilate_paragraph(source, line).unwrap(), None);
    }
}
