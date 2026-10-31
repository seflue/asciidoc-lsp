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
    let group_lines = &lines[group.clone()];
    let contents: Vec<&str> = group_lines
        .iter()
        .map(|span| &source[span.content.clone()])
        .collect();

    if layout.continues_item(group.start)
        || layout.ruled_out_above(group.start)
        // A block line right below a paragraph line is text of that paragraph.
        || layout.is_block_line(group.end)
        || sets_hardbreaks_option(tree.root_node(), source)
        || has_hard_line_break(&contents)
        || has_error_near(tree.root_node(), &group)
    {
        return Ok(None);
    }
    let Some(joined) = join_lines(&contents)? else {
        return Ok(None);
    };
    let range = group_lines[0].content.start..group_lines[group_lines.len() - 1].content.end;
    // The last line of a document may lack a line ending; take the group's
    // first one, else the document's.
    let eol = group_lines
        .iter()
        .chain(&lines)
        .map(|span| span.eol)
        .find(|eol| !eol.is_empty())
        .unwrap_or("\n");

    let starts = starts_outside_spans(
        &joined.text,
        sentence_starts(&joined.text),
        &joined.protected,
    );
    let starts = tailored_starts(&joined.text, starts);
    let new_text = render(&joined.text, &starts, eol);

    if new_text.is_empty() || new_text == source[range.clone()] {
        return Ok(None);
    }
    Ok(Some(VentilateEdit { range, new_text }))
}

/// Candidate sentence starts in `text` (byte offsets, ascending, never 0):
/// UAX #29 sentence boundaries, plus one after each `…` and its whitespace,
/// which UAX #29 does not treat as a sentence terminator.
fn sentence_starts(text: &str) -> Vec<usize> {
    let after_ellipsis = text.match_indices('…').filter_map(|(i, ellipsis)| {
        let rest = &text[i + ellipsis.len()..];
        let start = text.len() - rest.trim_start().len();
        (start > i + ellipsis.len() && start < text.len()).then_some(start)
    });
    let mut starts: Vec<usize> = text
        .split_sentence_bound_indices()
        .map(|(start, _)| start)
        .filter(|&start| start > 0)
        .chain(after_ellipsis)
        .collect();
    starts.sort_unstable();
    starts.dedup();
    starts
}

/// Move sentence starts out of protected spans. A start inside a span is
/// dropped, unless only markup follows it there: then the sentence ends with
/// the span, and the next one starts after the whitespace behind it.
fn starts_outside_spans(text: &str, starts: Vec<usize>, protected: &[Range<usize>]) -> Vec<usize> {
    let mut kept = Vec::new();
    for start in starts {
        match protected
            .iter()
            .find(|span| span.start < start && start < span.end)
        {
            None => kept.push(start),
            Some(span) if !text[start..span.end].chars().any(char::is_alphanumeric) => {
                let after = text.len() - text[span.end..].trim_start().len();
                if span.end < after && after < text.len() {
                    kept.push(after);
                }
            }
            Some(_) => {}
        }
    }
    kept.dedup();
    kept
}

/// Abbreviations (without their period) that never end a sentence, English
/// and German together.
const NEVER_SENTENCE_END: &[&str] = &[
    "Dr", "Mr", "Mrs", "Ms", "Prof", "e.g", "i.e", "vs", "z", "B", "d", "h", "bzw", "S", "Abschn",
    "ca", "vgl",
];

/// Abbreviations (without their period) that abbreviate only before a number.
const BEFORE_NUMBER: &[&str] = &["No", "Nr", "Fig", "Abb"];

/// Abbreviations (without their period) that end a sentence only before an
/// uppercase letter.
const MAY_END_SENTENCE: &[&str] = &["etc", "usw"];

/// German month names: a number with a period before one is an ordinal date
/// (`am 30. Oktober`), not a sentence end.
const GERMAN_MONTHS: &[&str] = &[
    "Januar",
    "Februar",
    "März",
    "April",
    "Mai",
    "Juni",
    "Juli",
    "August",
    "September",
    "Oktober",
    "November",
    "Dezember",
];

/// Drop sentence starts that UAX #29 puts after a period that does not end
/// a sentence: abbreviations, initials, numbers.
fn tailored_starts(text: &str, starts: Vec<usize>) -> Vec<usize> {
    starts
        .into_iter()
        .filter(|&start| ends_sentence(&text[..start], &text[start..]))
        .collect()
}

/// Whether the period-and-whitespace at the end of `before` ends a sentence,
/// given the text `after` it.
fn ends_sentence(before: &str, after: &str) -> bool {
    let trimmed = before.trim_end();
    if trimmed.len() == before.len() {
        return true;
    }
    let next = after.chars().next();
    let before_uppercase = next.is_some_and(char::is_uppercase);
    if trimmed.ends_with("...") || trimmed.ends_with('…') {
        return before_uppercase;
    }
    let Some(stem) = trimmed.strip_suffix('.') else {
        return true;
    };
    let token = stem
        .rsplit(char::is_whitespace)
        .next()
        .unwrap_or(stem)
        .trim_start_matches(|c: char| !c.is_alphanumeric());
    let is_initial = {
        let mut chars = token.chars();
        chars.next().is_some_and(char::is_uppercase) && chars.next().is_none()
    };
    let is_ordinal_date = !token.is_empty()
        && token.chars().all(|c| c.is_ascii_digit())
        && GERMAN_MONTHS.contains(
            &after
                .split(|c: char| !c.is_alphabetic())
                .next()
                .unwrap_or(""),
        );
    if MAY_END_SENTENCE.contains(&token) {
        return before_uppercase;
    }
    !(NEVER_SENTENCE_END.contains(&token)
        || is_initial
        || is_ordinal_date
        || BEFORE_NUMBER.contains(&token) && next.is_some_and(|c| c.is_ascii_digit()))
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

/// Whether the document sets `:hardbreaks-option:`.
fn sets_hardbreaks_option(root: tree_sitter::Node, source: &str) -> bool {
    crate::extractors::extract_attributes(root, source.as_bytes())
        .iter()
        .any(|attr| attr.name == "hardbreaks-option")
}

/// Whether any of `lines` ends in ` +`, a hard line break.
fn has_hard_line_break(lines: &[&str]) -> bool {
    lines.iter().any(|line| line.trim_end().ends_with(" +"))
}

/// Whether the block tree has an error in `group` or right above or below
/// it: an error there may be the rest of this paragraph.
fn has_error_near(root: tree_sitter::Node, group: &Range<usize>) -> bool {
    let mut error_rows = Vec::new();
    walk_for_error_rows(root, &mut error_rows);
    let neighbourhood = group.start.saturating_sub(1)..group.end + 1;
    error_rows
        .iter()
        .any(|rows| rows.start < neighbourhood.end && neighbourhood.start < rows.end)
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

/// A paragraph's lines joined into one, with the byte ranges in it that no
/// sentence may end inside.
struct JoinedParagraph {
    text: String,
    protected: Vec<Range<usize>>,
}

/// Join `lines` with single spaces and collect their protected spans.
/// `None` if the inline grammar fails on any line.
fn join_lines(lines: &[&str]) -> Result<Option<JoinedParagraph>, CoreError> {
    let mut parser = crate::parser::init_inline_parser()?;
    let mut joined = JoinedParagraph {
        text: String::new(),
        protected: Vec::new(),
    };
    for line in lines {
        let tree = crate::parser::parse(&mut parser, line)?;
        if tree.root_node().has_error() {
            return Ok(None);
        }
        let trimmed = line.trim();
        let lead = line.len() - line.trim_start().len();
        if !joined.text.is_empty() {
            joined.text.push(' ');
        }
        let base = joined.text.len();
        let mut spans = Vec::new();
        walk_for_protected(tree.root_node(), line, &mut spans);
        let to_joined = |offset: usize| base + offset.clamp(lead, lead + trimmed.len()) - lead;
        joined.protected.extend(
            spans
                .into_iter()
                .map(|span| to_joined(span.start)..to_joined(span.end)),
        );
        joined.text.push_str(trimmed);
    }
    Ok(Some(joined))
}

/// Byte ranges of inline markup in `line` that no sentence may end inside.
fn walk_for_protected(node: tree_sitter::Node, line: &str, spans: &mut Vec<Range<usize>>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            // `ltalic` is the grammar's spelling of italic.
            "inline_macro" | "xref" | "monospace" | "passthrough" | "macro_passthrough"
            | "stem_macro" | "footnote" | "id_assignment" | "index_term" | "index_term2"
            | "labled_uri" | "emphasis" | "ltalic" | "highlight" => {
                spans.push(child.byte_range());
            }
            // The grammar's autolink can run past the URL; end it ourselves,
            // unless a label in brackets closes it.
            "autolink" if child.child(0).is_some_and(|uri| uri.kind() == "labled_uri") => {
                walk_for_protected(child, line, spans);
            }
            "autolink" => spans.push(child.start_byte()..url_end(line, child.start_byte())),
            _ => walk_for_protected(child, line, spans),
        }
    }
}

/// End of the URL starting at `start`: the first whitespace, without
/// trailing punctuation.
fn url_end(line: &str, start: usize) -> usize {
    let rest = &line[start..];
    let url = rest.split(char::is_whitespace).next().unwrap_or(rest);
    start
        + url
            .trim_end_matches(['.', ',', ';', ':', '!', '?', ')'])
            .len()
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
    #[case::two_sentences_on_one_line(
        "First sentence. Second sentence.\n",
        1,
        "First sentence.\nSecond sentence.\n"
    )]
    #[case::wrapped_sentence_is_joined(
        "This sentence was wrapped\nat seventy-two columns by an editor.\n",
        1,
        "This sentence was wrapped at seventy-two columns by an editor.\n"
    )]
    #[case::wrapped_and_multiple_sentences(
        "One. Two is\nwrapped. Three.\n",
        1,
        "One.\nTwo is wrapped.\nThree.\n"
    )]
    #[case::already_ventilated("One.\nTwo.\nThree.\n", 2, "One.\nTwo.\nThree.\n")]
    #[case::single_sentence("Just one sentence.\n", 1, "Just one sentence.\n")]
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
    #[case::question_and_exclamation("Is it? Yes! Done.\n", 1, "Is it?\nYes!\nDone.\n")]
    #[case::colon_and_semicolon_do_not_split(
        "Two options: fast; slow. End.\n",
        1,
        "Two options: fast; slow.\nEnd.\n"
    )]
    #[case::multiple_spaces_after_period("First.   Second.\n", 1, "First.\nSecond.\n")]
    #[case::trailing_whitespace_on_wrapped_line(
        "Wrapped here   \nand joined.\n",
        1,
        "Wrapped here and joined.\n"
    )]
    #[case::whitespace_inside_line_is_kept(
        "Tab\there and  two spaces.\nNext\tline.\n",
        1,
        "Tab\there and  two spaces.\nNext\tline.\n"
    )]
    #[case::crlf_stays_crlf(
        "One. Two is\r\nwrapped. Three.\r\n",
        1,
        "One.\r\nTwo is wrapped.\r\nThree.\r\n"
    )]
    #[case::umlauts(
        "Größe ändern. Übernächster Satz.\n",
        1,
        "Größe ändern.\nÜbernächster Satz.\n"
    )]
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
    #[case::crlf_paragraph_without_final_newline(
        "A.\r\n\r\nOne. Two.",
        3,
        "A.\r\n\r\nOne.\r\nTwo."
    )]
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
    // A sentence end at the end of a formatting span splits after it
    #[case::sentence_end_inside_bold("*Done.* Next.\n", 1, "*Done.*\nNext.\n")]
    #[case::xref_macro_text(
        "See xref:setup.adoc#install[Install. Guide] first. Next.\n",
        1,
        "See xref:setup.adoc#install[Install. Guide] first.\nNext.\n"
    )]
    #[case::xref_shorthand_text(
        "See <<sec,Sec. Two>> here. Next.\n",
        1,
        "See <<sec,Sec. Two>> here.\nNext.\n"
    )]
    #[case::bare_url(
        "Visit https://example.com/a.B/c for more. Next.\n",
        1,
        "Visit https://example.com/a.B/c for more.\nNext.\n"
    )]
    #[case::url_with_label(
        "See https://x.org[Read. This] now. Next.\n",
        1,
        "See https://x.org[Read. This] now.\nNext.\n"
    )]
    #[case::stem_and_index_term(
        "Use stem:[a. B] and ((Index. Term)) now. Next.\n",
        1,
        "Use stem:[a. B] and ((Index. Term)) now.\nNext.\n"
    )]
    #[case::url_at_sentence_end(
        "Visit https://example.com. Then log in.\n",
        1,
        "Visit https://example.com.\nThen log in.\n"
    )]
    #[case::link_macro_text(
        "link:https://x.org[Read this. Now.] today. Next.\n",
        1,
        "link:https://x.org[Read this. Now.] today.\nNext.\n"
    )]
    #[case::monospace("Run `a. B` now. Next.\n", 1, "Run `a. B` now.\nNext.\n")]
    #[case::passthrough(
        "Use +a. B+ and pass:[a. B] here. Next.\n",
        1,
        "Use +a. B+ and pass:[a. B] here.\nNext.\n"
    )]
    #[case::attribute_reference_at_sentence_end(
        "{product-name}. Next.\n",
        1,
        "{product-name}.\nNext.\n"
    )]
    #[case::footnote(
        "Claim.footnote:[Source. Page 3.] Next.\n",
        1,
        "Claim.footnote:[Source. Page 3.]\nNext.\n"
    )]
    #[case::inline_anchor("[[id]]First. Second.\n", 1, "[[id]]First.\nSecond.\n")]
    #[case::image_macro("image:icon.png[] Done. Next.\n", 1, "image:icon.png[] Done.\nNext.\n")]
    #[case::bold_spanning_sentences(
        "*Bold one. Bold two.* After.\n",
        1,
        "*Bold one. Bold two.*\nAfter.\n"
    )]
    #[case::italic_and_highlight(
        "_Ital. Two._ More. #Mark. Two.# End.\n",
        1,
        "_Ital. Two._\nMore.\n#Mark. Two.#\nEnd.\n"
    )]
    #[case::unconstrained_bold("**Un. Two.**ab more. End.\n", 1, "**Un. Two.**ab more.\nEnd.\n")]
    #[case::span_on_wrapped_line(
        "First is\nwrapped `a. B` here. Next.\n",
        1,
        "First is wrapped `a. B` here.\nNext.\n"
    )]
    #[case::latin_abbreviations_before_lowercase(
        "Use a tool, e.g. pandoc, i.e. a converter. Next.\n",
        1,
        "Use a tool, e.g. pandoc, i.e. a converter.\nNext.\n"
    )]
    #[case::latin_abbreviation_before_uppercase(
        "Use e.g. Pandoc here. Next.\n",
        1,
        "Use e.g. Pandoc here.\nNext.\n"
    )]
    #[case::title_abbreviations(
        "Ask Dr. Smith or Mr. Jones. Next.\n",
        1,
        "Ask Dr. Smith or Mr. Jones.\nNext.\n"
    )]
    #[case::german_abbreviations(
        "Das ist z. B. ein Fall, d. h. ein Beispiel. Siehe S. 12 bzw. Abschn. 3. Weiter.\n",
        1,
        "Das ist z. B. ein Fall, d. h. ein Beispiel.\nSiehe S. 12 bzw. Abschn. 3.\nWeiter.\n"
    )]
    #[case::english_and_german_abbreviations_together(
        "Ask Prof. Weber, vgl. Dr. Smith. Then ca. Zehn vs. Mrs. Jones. End.\n",
        1,
        "Ask Prof. Weber, vgl. Dr. Smith.\nThen ca. Zehn vs. Mrs. Jones.\nEnd.\n"
    )]
    #[case::initials(
        "Written by J. R. R. Tolkien. Next.\n",
        1,
        "Written by J. R. R. Tolkien.\nNext.\n"
    )]
    #[case::number_abbreviations_before_digit(
        "See No. 5. Then Fig. 2 and Nr. 7. Siehe Abb. 3. Dann.\n",
        1,
        "See No. 5.\nThen Fig. 2 and Nr. 7.\nSiehe Abb. 3.\nDann.\n"
    )]
    #[case::number_abbreviation_at_sentence_end(
        "She said No. Then she left.\n",
        1,
        "She said No.\nThen she left.\n"
    )]
    #[case::etc_before_uppercase(
        "Apples, pears, etc. Next sentence.\n",
        1,
        "Apples, pears, etc.\nNext sentence.\n"
    )]
    #[case::etc_and_usw_before_lowercase_and_digit(
        "Apples, etc. and pears. Äpfel usw. 3. Weiter.\n",
        1,
        "Apples, etc. and pears.\nÄpfel usw. 3.\nWeiter.\n"
    )]
    #[case::usw_before_uppercase(
        "Äpfel, Birnen usw. Dann mehr.\n",
        1,
        "Äpfel, Birnen usw.\nDann mehr.\n"
    )]
    #[case::ellipsis_before_lowercase(
        "Wait... what happened? Nothing.\n",
        1,
        "Wait... what happened?\nNothing.\n"
    )]
    #[case::ellipsis_before_uppercase(
        "Wait... What happened? Nothing.\n",
        1,
        "Wait...\nWhat happened?\nNothing.\n"
    )]
    #[case::ellipsis_character_before_lowercase(
        "Wait… what happened? Nothing.\n",
        1,
        "Wait… what happened?\nNothing.\n"
    )]
    #[case::ellipsis_character_before_uppercase(
        "Wait… What happened? Nothing.\n",
        1,
        "Wait…\nWhat happened?\nNothing.\n"
    )]
    #[case::ellipsis_character_inside_monospace(
        "Run `a… B` now. Next.\n",
        1,
        "Run `a… B` now.\nNext.\n"
    )]
    #[case::ellipsis_before_digit("Count... 3 more. Done.\n", 1, "Count... 3 more.\nDone.\n")]
    #[case::german_ordinal_date(
        "Am 30. Oktober. Dann am 1. Mai und 3. März. Ende.\n",
        1,
        "Am 30. Oktober.\nDann am 1. Mai und 3. März.\nEnde.\n"
    )]
    #[case::number_before_capitalised_word(
        "Siehe Seite 12. Dort steht es.\n",
        1,
        "Siehe Seite 12.\nDort steht es.\n"
    )]
    #[case::section_number_at_sentence_end(
        "See section 2.3. Then continue.\n",
        1,
        "See section 2.3.\nThen continue.\n"
    )]
    #[case::decimal_version_and_ip(
        "Costs 3.50 euros. Requires pandoc 3.1.2 or later. Connect to 10.0.0.1 now. End.\n",
        1,
        "Costs 3.50 euros.\nRequires pandoc 3.1.2 or later.\nConnect to 10.0.0.1 now.\nEnd.\n"
    )]
    #[case::closing_quote_and_parenthesis(
        "He said \"stop.\" Then he left. (This is an aside.) Next.\n",
        1,
        "He said \"stop.\"\nThen he left.\n(This is an aside.)\nNext.\n"
    )]
    #[case::german_quotes(
        "Er sagte „Halt.“ Dann ging er.\n",
        1,
        "Er sagte „Halt.“\nDann ging er.\n"
    )]
    #[case::abbreviation_in_parenthesis(
        "Use a tool (e.g. Pandoc) here. Next.\n",
        1,
        "Use a tool (e.g. Pandoc) here.\nNext.\n"
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
    #[case::empty_document("", 1)]
    #[case::whitespace_only_document("  \n\t\n", 1)]
    #[case::cursor_on_blank_line("First. Second.\n\nThird.\n", 2)]
    #[case::cursor_on_section_title("== Title. With dot\n\nFirst. Second.\n", 1)]
    #[case::cursor_past_end("First. Second.\n", 5)]
    #[case::already_ventilated("One.\nTwo.\n", 1)]
    #[case::list_item("* Item one. Two.\n", 1)]
    #[case::list_item_continuation("* Item one. Two.\ncontinued here. More.\n", 2)]
    #[case::ordered_list_item_continuation(". Item one. Two.\ncontinued here. More.\n", 2)]
    #[case::description_list_item_continuation("CPU:: Def one. Two.\nmore def. Yes.\n", 2)]
    #[case::block_attached_to_list_item("* Item.\n+\nAttached one. Two.\n", 3)]
    #[case::admonition("NOTE: First. Second.\n", 1)]
    #[case::admonition_continuation("NOTE: First. Second.\ncontinued. Yes.\n", 2)]
    // The block attribute applies to every line of the group
    #[case::verse_style_on_first_line("[verse]\nLine one. Two.\nLine three.\n", 2)]
    #[case::verse_style_on_later_line("[verse]\nLine one. Two.\nLine three.\n", 3)]
    #[case::source_style("[source]\nlet a = 1. Two.\n", 2)]
    #[case::source_style_with_language("[source,rust]\nlet a = 1. Two.\n", 2)]
    #[case::literal_style("[literal]\nOne. Two.\n", 2)]
    #[case::pass_style("[pass]\nOne. Two.\n", 2)]
    #[case::listing_style("[listing]\nOne. Two.\n", 2)]
    #[case::verse_with_author("[verse,Carl Sandburg,Fog]\nOne. Two.\n", 2)]
    #[case::verse_below_block_title(".Fog\n[verse]\nOne. Two.\n", 3)]
    // A block attribute or title line inside a paragraph is text
    #[case::attribute_line_inside_paragraph_above("One. Two.\n[x]\nThree. Four.\n", 1)]
    #[case::attribute_line_inside_paragraph_below("One. Two.\n[x]\nThree. Four.\n", 3)]
    #[case::anchor_line_inside_paragraph("One. Two.\n[[id]]\nThree. Four.\n", 3)]
    #[case::title_line_inside_paragraph("One. Two.\n.Title\nThree. Four.\n", 1)]
    // Admonition and quote styles
    #[case::note_style("[NOTE]\nOne. Two.\n", 2)]
    #[case::caution_style("[CAUTION]\nOne. Two.\n", 2)]
    #[case::quote_style("[quote]\nOne. Two.\n", 2)]
    #[case::admonition_block("[NOTE]\n====\nOne. Two.\n====\n", 3)]
    #[case::verse_styled_sidebar("[verse]\n****\nOne. Two.\n****\n", 3)]
    #[case::quote_style_with_attribution("[quote,Author,Source]\nOne. Two.\n", 2)]
    #[case::hardbreaks_option("[%hardbreaks]\nOne. Two.\nThree.\n", 3)]
    #[case::hardbreaks_option_after_style("[normal%hardbreaks]\nOne. Two.\n", 2)]
    #[case::hardbreaks_named_options("[options=hardbreaks]\nOne. Two.\n", 2)]
    #[case::hardbreaks_quoted_opts("[opts=\"autofit,hardbreaks\"]\nOne. Two.\n", 2)]
    #[case::hardbreaks_after_id("[#intro%hardbreaks]\nOne. Two.\n", 2)]
    #[case::cursor_on_block_anchor("[[sec-a]]\nFirst. Second.\n", 1)]
    // The grammar gives these no paragraph node
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
    #[case::block_parse_error_right_below("One. Two.\n[unclosed\nThree. Four.\n", 1)]
    #[case::bold_spanning_two_lines("Some *bold\ntext* here. Two.\n", 1)]
    #[case::description_list_with_lowercase_term("term:: Def. Two.\nmore. Yes.\n", 2)]
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
