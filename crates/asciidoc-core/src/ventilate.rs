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
        // So is a list item line; formatting would cover only part of the paragraph.
        || layout.starts_item(group.end)
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
    let starts = with_forced_starts(starts, &joined);
    let starts = starts_off_block_syntax(&joined.text, starts);
    let new_text = render(&joined.text, &starts, eol);

    // Joining builds the first line anew; on a paragraph's first line
    // Asciidoctor reads more as block syntax than on the lines after it.
    let first_line = new_text.split(eol).next().unwrap_or("");
    if new_text.is_empty()
        || new_text == source[range.clone()]
        || opens_block_on_first_line(first_line)
    {
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

/// Add the starts the source fixes to `starts`, keeping them ascending.
fn with_forced_starts(mut starts: Vec<usize>, joined: &JoinedParagraph) -> Vec<usize> {
    starts.extend(
        joined
            .forced_starts
            .iter()
            .filter(|&&start| start > 0 && start < joined.text.len()),
    );
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
/// and German together. Multi-part abbreviations appear with and without the
/// inner space (`z. B.`, `z.B.`).
const NEVER_SENTENCE_END: &[&str] = &[
    "Dr", "Mr", "Mrs", "Ms", "Prof", "e.g", "i.e", "vs", "z", "B", "z.B", "d", "h", "d.h", "bzw",
    "S", "Abschn", "ca", "vgl", "u.a", "z.T", "o.ä", "s.o", "u.U", "v.a",
];

/// Multi-part abbreviations written with a space between their parts
/// (`u. a.`), as their parts without periods. A part never ends a sentence
/// only inside the whole sequence: `u.` or `a.` alone still may.
const SPACED_NEVER_SENTENCE_END: &[&[&str]] = &[
    &["e", "g"],
    &["i", "e"],
    &["z", "B"],
    &["d", "h"],
    &["u", "a"],
    &["z", "T"],
    &["o", "ä"],
    &["s", "o"],
    &["u", "U"],
    &["v", "a"],
];

/// Whether the period-and-whitespace at the end of `before` lies inside or at
/// the end of a spaced multi-part abbreviation, given the text `after` it.
fn in_spaced_abbreviation(before: &str, after: &str) -> bool {
    let before_tokens: Vec<&str> = before.split_whitespace().collect();
    let after_tokens: Vec<&str> = after.split_whitespace().collect();
    SPACED_NEVER_SENTENCE_END.iter().any(|parts| {
        (1..=parts.len()).any(|k| {
            let (head, tail) = parts.split_at(k);
            let Some(start) = before_tokens.len().checked_sub(k) else {
                return false;
            };
            let head_matches =
                before_tokens[start..]
                    .iter()
                    .enumerate()
                    .zip(head)
                    .all(|((i, token), part)| {
                        // Only the sequence's first part may follow a bracket or quote.
                        let token = if i == 0 {
                            token.trim_start_matches(|c: char| !c.is_alphanumeric())
                        } else {
                            token
                        };
                        token.strip_suffix('.') == Some(part)
                    });
            let tail_matches = tail.len() <= after_tokens.len()
                && after_tokens.iter().zip(tail).all(|(token, part)| {
                    token
                        .strip_prefix(part)
                        .and_then(|rest| rest.strip_prefix('.'))
                        .is_some_and(|rest| !rest.chars().any(char::is_alphanumeric))
                });
            head_matches && tail_matches
        })
    })
}

/// Abbreviations (without their period) that abbreviate only before a number.
const BEFORE_NUMBER: &[&str] = &["No", "Nr", "Fig", "Abb"];

/// Abbreviations (without their period) that end a sentence only before an
/// uppercase letter.
const MAY_END_SENTENCE: &[&str] = &["etc", "usw"];

/// Opening quotes, brackets and inline markup characters that may stand
/// before the first letter of a sentence (`"Next"`, `(Siehe …)`, `*Bold*`).
const OPENING_PUNCTUATION_AND_MARKUP: &[char] = &[
    '"', '\'', '„', '“', '‚', '‘', '«', '»', '‹', '›', '(', '[', '*', '_', '#', '`', '+', '~', '^',
];

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
    // Without whitespace there, a line break would add some to the text.
    if trimmed.len() == before.len() {
        return false;
    }
    let next = after.chars().next();
    let before_uppercase = after
        .trim_start_matches(OPENING_PUNCTUATION_AND_MARKUP)
        .starts_with(char::is_uppercase);
    if trimmed.ends_with("...") || trimmed.ends_with('…') {
        return before_uppercase;
    }
    let Some(stem) = trimmed.strip_suffix('.') else {
        return true;
    };
    if in_spaced_abbreviation(before, after) {
        return false;
    }
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

/// Drop sentence starts whose line Asciidoctor would read as block syntax;
/// that sentence stays on the line before. Goes from the last start back, so
/// each line is known up to the next start that is kept.
fn starts_off_block_syntax(text: &str, starts: Vec<usize>) -> Vec<usize> {
    let mut kept = Vec::new();
    let mut line_end = text.len();
    for start in starts.into_iter().rev() {
        if !reads_as_block_syntax(text[start..line_end].trim_end()) {
            kept.push(start);
            line_end = start;
        }
    }
    kept.reverse();
    kept
}

/// Admonition labels that open an admonition paragraph at the start of a line.
const ADMONITION_LABELS: &[&str] = &["NOTE:", "TIP:", "IMPORTANT:", "WARNING:", "CAUTION:"];

/// Whether Asciidoctor would read `line`, at the start of a line, as block
/// syntax rather than paragraph text.
fn reads_as_block_syntax(line: &str) -> bool {
    line.starts_with("//")
        || line.starts_with('[')
        || line == "+"
        || is_list_marker(line)
        || is_delimiter_line(line)
        || ADMONITION_LABELS.iter().any(|label| line.starts_with(label))
        || is_attribute_entry(line)
        || is_preprocessor_directive(line)
        // Block title: a period followed by anything but whitespace.
        || line
            .strip_prefix('.')
            .is_some_and(|rest| rest.starts_with(|c: char| !c.is_whitespace()))
}

/// Whether `line` starts with a list marker and a space or tab: `*`, `-`,
/// `.` runs, `1.`, `a.`, or a callout `<1>`.
fn is_list_marker(line: &str) -> bool {
    let Some((marker, _)) = line.split_once([' ', '\t']) else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    let ordinal = marker.strip_suffix('.').unwrap_or("");
    (!marker.is_empty() && marker.chars().all(|c| c == '*'))
        || marker == "-"
        || (!marker.is_empty() && marker.chars().all(|c| c == '.'))
        || digits(ordinal)
        || (ordinal.len() == 1 && ordinal.chars().all(|c| c.is_ascii_alphabetic()))
        || marker
            .strip_prefix('<')
            .and_then(|m| m.strip_suffix('>'))
            .is_some_and(digits)
}

/// Whether `line` is a block delimiter line: an open block `--`, a fenced
/// code block with an optional language, a table, or a run of four or more
/// delimiter characters.
fn is_delimiter_line(line: &str) -> bool {
    if line == "--" || line.starts_with("```") {
        return true;
    }
    if let Some(rest) = ["|===", ",===", ":===", "!==="]
        .iter()
        .find_map(|table| line.strip_prefix(table))
    {
        return rest.chars().all(|c| c == '=');
    }
    ['-', '.', '*', '=', '_', '+', '/']
        .iter()
        .any(|&delimiter| line.len() >= 4 && line.chars().all(|c| c == delimiter))
}

/// Whether `line` starts with an attribute entry, `:name:` followed by
/// whitespace or the end of the line.
fn is_attribute_entry(line: &str) -> bool {
    let Some(rest) = line.strip_prefix(':') else {
        return false;
    };
    let Some((name, after)) = rest.split_once(':') else {
        return false;
    };
    name.trim_start_matches('!')
        .starts_with(|c: char| c.is_alphanumeric() || c == '_')
        && (after.is_empty() || after.starts_with(char::is_whitespace))
}

/// Whether `line` is a preprocessor directive, which Asciidoctor reads on
/// any line, inside a paragraph too: `include::`, `ifdef::`, `ifndef::`,
/// `ifeval::` or `endif::`, up to a closing `]` at the end of the line.
fn is_preprocessor_directive(line: &str) -> bool {
    ["include::", "ifdef::", "ifndef::", "ifeval::", "endif::"]
        .iter()
        .any(|directive| line.starts_with(directive))
        && line.ends_with(']')
}

/// Whether Asciidoctor would read `line`, as the first line of a paragraph,
/// as the start of a block: a list item, an admonition, a description list
/// term, a block macro or preprocessor directive, or a block attribute line.
fn opens_block_on_first_line(line: &str) -> bool {
    is_list_marker(line)
        || ADMONITION_LABELS.iter().any(|label| {
            line.strip_prefix(label)
                .is_some_and(|rest| rest.starts_with([' ', '\t']))
        })
        || is_description_list_term(line)
        || is_block_macro(line)
        || block_attribute_line(line).is_some()
}

/// Whether `line` holds a description list term: text, then `::`, `:::`,
/// `::::` or `;;` followed by whitespace or the end of the line.
fn is_description_list_term(line: &str) -> bool {
    ["::", ";;"].iter().any(|delimiter| {
        // Overlapping matches: `:::` ends in a `::` that starts at its second colon.
        line.char_indices().any(|(i, _)| {
            if !line[i..].starts_with(delimiter) {
                return false;
            }
            let after = &line[i + delimiter.len()..];
            !line[..i].trim().is_empty() && (after.is_empty() || after.starts_with([' ', '\t']))
        })
    })
}

/// Whether `line` is a block macro, `name::target[attributes]`, which
/// includes the preprocessor directives.
fn is_block_macro(line: &str) -> bool {
    let Some((name, rest)) = line.split_once("::") else {
        return false;
    };
    name.starts_with(|c: char| c.is_alphanumeric() || c == '_')
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        && rest.contains('[')
        && rest.ends_with(']')
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

    fn starts_item(&self, row: usize) -> bool {
        self.items.iter().any(|rows| rows.start == row)
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
                let mut lines = child.walk();
                for line in child.children(&mut lines).filter(|n| n.kind() == "line") {
                    let row = line.start_position().row;
                    match block_attribute_line(source[line.byte_range()].trim()) {
                        Some(attr) => layout.attributes.push((row, attr)),
                        None => layout.paragraphs.push(row),
                    }
                }
            }
            "element_attr" => {
                let attr = block_attribute_line(source[child.byte_range()].trim()).unwrap_or("");
                layout.attributes.push((row, attr));
            }
            "block_title" => layout.titles.push(row),
            "list" | "description_list" | "admonition" => layout.items.push(rows_of(child)),
            "block_element" | "section" | "section_block" => walk_for_layout(child, source, layout),
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

/// Whether any of `lines` ends in a blank and `+`, a hard line break.
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
    /// Sentence starts the source fixes: before and after each label line.
    forced_starts: Vec<usize>,
}

/// Join `lines` with single spaces and collect their protected spans.
/// `None` if the inline grammar fails on any line.
fn join_lines(lines: &[&str]) -> Result<Option<JoinedParagraph>, CoreError> {
    let mut parser = crate::parser::init_inline_parser()?;
    let mut joined = JoinedParagraph {
        text: String::new(),
        protected: Vec::new(),
        forced_starts: Vec::new(),
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
                .iter()
                .map(|span| to_joined(span.start)..to_joined(span.end)),
        );
        if is_label_line(trimmed, &spans, lead) {
            joined.forced_starts.push(base);
            joined.forced_starts.push(base + trimmed.len() + 1);
        }
        joined.text.push_str(trimmed);
    }
    Ok(Some(joined))
}

/// A line holding only one bold or italic span whose text ends in `:`,
/// such as `*Context:*`: a label for the text below it, not part of a sentence.
fn is_label_line(trimmed: &str, spans: &[Range<usize>], lead: usize) -> bool {
    trimmed.starts_with(['*', '_'])
        && trimmed.trim_end_matches(['*', '_']).ends_with(':')
        && spans.contains(&(lead..lead + trimmed.len()))
}

/// Byte ranges of inline markup in `line` that no sentence may end inside.
fn walk_for_protected(node: tree_sitter::Node, line: &str, spans: &mut Vec<Range<usize>>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "inline_macro" | "xref" | "monospace" | "passthrough" | "macro_passthrough"
            | "stem_macro" | "footnote" | "id_assignment" | "index_term" | "index_term2"
            | "labled_uri" | "emphasis" | "italic" | "highlight" => {
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
    #[case::image_macro_alt_text(
        "See image:icon.png[Alt. Text] here. Next.\n",
        1,
        "See image:icon.png[Alt. Text] here.\nNext.\n"
    )]
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
    #[case::german_abbreviations_without_inner_space(
        "Das ist z.B. Pandoc, d.h. Ein Tool. Es kann u.a. Dateien, z.T. Bilder, o.ä. Formate. Siehe s.o. Dort u.U. Mehr, v.a. Text. Weiter.\n",
        1,
        "Das ist z.B. Pandoc, d.h. Ein Tool.\nEs kann u.a. Dateien, z.T. Bilder, o.ä. Formate.\nSiehe s.o. Dort u.U. Mehr, v.a. Text.\nWeiter.\n"
    )]
    #[case::spaced_multi_part_abbreviations(
        "Es kann u. a. Dateien, z. T. Bilder, o. ä. Formate. Siehe s. o. Dort u. U. Mehr, v. a. Text. Use e. g. Pandoc, i. e. Docs. Weiter.\n",
        1,
        "Es kann u. a. Dateien, z. T. Bilder, o. ä. Formate.\nSiehe s. o. Dort u. U. Mehr, v. a. Text.\nUse e. g. Pandoc, i. e. Docs.\nWeiter.\n"
    )]
    #[case::single_parts_of_spaced_abbreviations_end_sentences(
        "Gruppe u. Dann weiter. Spalte a. Berlin liegt. Siehe s. Oben steht es. Ende.\n",
        1,
        "Gruppe u.\nDann weiter.\nSpalte a.\nBerlin liegt.\nSiehe s.\nOben steht es.\nEnde.\n"
    )]
    #[case::compact_abbreviation_in_parenthesis(
        "Ein Tool (z.B. Pandoc) hier. Weiter.\n",
        1,
        "Ein Tool (z.B. Pandoc) hier.\nWeiter.\n"
    )]
    #[case::unlisted_compact_token_ends_sentence(
        "Siehe Anhang A.B. Dann weiter.\n",
        1,
        "Siehe Anhang A.B.\nDann weiter.\n"
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
    #[case::etc_before_quoted_uppercase(
        "Apples, etc. \"Next\" is a word.\n",
        1,
        "Apples, etc.\n\"Next\" is a word.\n"
    )]
    #[case::etc_before_german_quoted_uppercase(
        "Äpfel etc. „Weiter“ ist ein Wort.\n",
        1,
        "Äpfel etc.\n„Weiter“ ist ein Wort.\n"
    )]
    #[case::usw_before_parenthesised_uppercase(
        "Äpfel usw. (Siehe unten.) Dann.\n",
        1,
        "Äpfel usw.\n(Siehe unten.)\nDann.\n"
    )]
    #[case::ellipsis_before_bold_uppercase(
        "Wait... *Bold* text. Next.\n",
        1,
        "Wait...\n*Bold* text.\nNext.\n"
    )]
    #[case::ellipsis_character_before_quoted_uppercase(
        "Wait… \"What\" now? Next.\n",
        1,
        "Wait…\n\"What\" now?\nNext.\n"
    )]
    #[case::etc_and_ellipsis_before_quoted_lowercase(
        "Apples, etc. \"and\" pears. Wait... *bold* text. Äpfel etc. „und“ Birnen. End.\n",
        1,
        "Apples, etc. \"and\" pears.\nWait... *bold* text.\nÄpfel etc. „und“ Birnen.\nEnd.\n"
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
    // A sentence that would start a line as block syntax stays on the line before
    #[case::line_comment_start("One. Two. // Three.\n", 1, "One.\nTwo. // Three.\n")]
    #[case::dash_list_marker_after_span("One. *Two.* - Three.\n", 1, "One.\n*Two.* - Three.\n")]
    #[case::ordered_list_markers(
        "One. Two. 3. 4 items. B. Four.\n",
        1,
        "One.\nTwo. 3. 4 items. B. Four.\n"
    )]
    #[case::callout_marker("One. Two. <1> Three.\n", 1, "One.\nTwo. <1> Three.\n")]
    #[case::admonition_labels(
        "One. Two. NOTE: A. TIP: B. IMPORTANT: C. WARNING: D. CAUTION: E.\n",
        1,
        "One.\nTwo. NOTE: A. TIP: B. IMPORTANT: C. WARNING: D. CAUTION: E.\n"
    )]
    #[case::block_title_and_dot_list_marker_after_span(
        "One. *Two.* .title here. *Three.* . item here.\n",
        1,
        "One.\n*Two.* .title here.\n*Three.* . item here.\n"
    )]
    #[case::attribute_entry_after_span(
        "One. *Two.* :Attr: Three.\n",
        1,
        "One.\n*Two.* :Attr: Three.\n"
    )]
    #[case::bracket_start("One. Two. [Three] here.\n", 1, "One.\nTwo. [Three] here.\n")]
    #[case::lone_plus("One. Two.\t+\n", 1, "One.\nTwo.\t+\n")]
    #[case::listing_delimiter_after_span("One. *Two.* ----\n", 1, "One.\n*Two.* ----\n")]
    #[case::sidebar_delimiter("One. Two. ****\n", 1, "One.\nTwo. ****\n")]
    #[case::example_delimiter("One. Two. ====\n", 1, "One.\nTwo. ====\n")]
    #[case::quote_delimiter("One. Two. ____\n", 1, "One.\nTwo. ____\n")]
    #[ignore = "grammar reads `++++` as an unclosed passthrough (adoc-0146)"]
    #[case::passthrough_delimiter("One. Two. ++++\n", 1, "One.\nTwo. ++++\n")]
    #[case::comment_delimiter("One. Two. ////\n", 1, "One.\nTwo. ////\n")]
    #[case::table_delimiter("One. Two. |===\n", 1, "One.\nTwo. |===\n")]
    #[case::literal_delimiter_after_span("One. *Two.* ....\n", 1, "One.\n*Two.* ....\n")]
    #[case::decimal_after_period_reads_as_block_title(
        "One. *Two.* .5 Liter here.\n",
        1,
        "One.\n*Two.* .5 Liter here.\n"
    )]
    #[case::dash_list_marker_followed_by_tab_after_span(
        "One. *Two.* -\tThree.\n",
        1,
        "One.\n*Two.* -\tThree.\n"
    )]
    #[case::ordered_list_marker_followed_by_tab(
        "One. Two. 3.\tthree. Four.\n",
        1,
        "One.\nTwo. 3.\tthree.\nFour.\n"
    )]
    #[case::callout_marker_followed_by_tab(
        "One. Two. <1>\tThree.\n",
        1,
        "One.\nTwo. <1>\tThree.\n"
    )]
    #[case::open_block_delimiter_after_span("One. *Two.* --\n", 1, "One.\n*Two.* --\n")]
    #[case::csv_table_delimiter_after_span("One. *Two.* ,===\n", 1, "One.\n*Two.* ,===\n")]
    #[case::dsv_table_delimiter_after_span("One. *Two.* :===\n", 1, "One.\n*Two.* :===\n")]
    #[case::nested_table_delimiter_after_span("One. *Two.* !===\n", 1, "One.\n*Two.* !===\n")]
    #[case::include_directive_after_question(
        "One. Two? include::part.adoc[]\n",
        1,
        "One.\nTwo? include::part.adoc[]\n"
    )]
    #[case::ifdef_directive_after_question(
        "One. Two? ifdef::env-x[]\n",
        1,
        "One.\nTwo? ifdef::env-x[]\n"
    )]
    #[case::ifndef_directive_after_question(
        "One. Two? ifndef::env-x[]\n",
        1,
        "One.\nTwo? ifndef::env-x[]\n"
    )]
    #[case::ifeval_directive_after_question(
        "One. Two? ifeval::[1 == 2]\n",
        1,
        "One.\nTwo? ifeval::[1 == 2]\n"
    )]
    #[case::endif_directive_after_question("One. Two? endif::[]\n", 1, "One.\nTwo? endif::[]\n")]
    // A sentence that only looks like block syntax starts its own line
    #[case::note_without_colon("One. NOTE that it works.\n", 1, "One.\nNOTE that it works.\n")]
    #[case::admonition_label_not_in_capitals(
        "One. Note: this works.\n",
        1,
        "One.\nNote: this works.\n"
    )]
    #[case::decimal_number_at_line_start("One. 3.5 kg here.\n", 1, "One.\n3.5 kg here.\n")]
    #[case::bold_at_line_start("One. *Bold* here.\n", 1, "One.\n*Bold* here.\n")]
    #[case::single_slash_at_line_start("One. /Data is here.\n", 1, "One.\n/Data is here.\n")]
    #[case::double_dash_with_text_after_span(
        "One. *Two.* -- Three.\n",
        1,
        "One.\n*Two.*\n-- Three.\n"
    )]
    #[case::table_delimiter_with_text_after_span(
        "One. *Two.* ,=== Here.\n",
        1,
        "One.\n*Two.*\n,=== Here.\n"
    )]
    #[case::directive_with_text_after_question(
        "One. Two? include::part.adoc[] here.\n",
        1,
        "One.\nTwo?\ninclude::part.adoc[] here.\n"
    )]
    #[case::escaped_directive_after_question(
        "One. Two? \\include::part.adoc[]\n",
        1,
        "One.\nTwo?\n\\include::part.adoc[]\n"
    )]
    #[case::toc_block_macro("One. Two? toc::[]\n", 1, "One.\nTwo?\ntoc::[]\n")]
    #[case::image_block_macro("One. Two? image::a.png[]\n", 1, "One.\nTwo?\nimage::a.png[]\n")]
    #[case::thematic_break("One. Two. '''\n", 1, "One.\nTwo.\n'''\n")]
    #[case::bold_spanning_two_lines(
        "Some *bold\ntext* here. Two.\n",
        1,
        "Some *bold text* here.\nTwo.\n"
    )]
    // Joined first lines that only look like block syntax
    #[case::join_with_single_colon("A\nb: c. D.\n", 1, "A b: c.\nD.\n")]
    #[ignore = "grammar starts a description list on line 2 (adoc-0135)"]
    #[case::description_list_term_on_second_line("One. Two\nb;; c.\n", 1, "One.\nTwo b;; c.\n")]
    // Without whitespace after the period a line break would change the text
    #[case::period_inside_word("Use .NET here. Next.\n", 1, "Use .NET here.\nNext.\n")]
    // Protected spans, tailoring and the line start guard together
    #[case::abbreviations_in_and_before_link_macro(
        "Ask Dr. link:https://x.org[e.g. Pandoc. Docs] now. Next.\n",
        1,
        "Ask Dr. link:https://x.org[e.g. Pandoc. Docs] now.\nNext.\n"
    )]
    #[case::url_at_sentence_end_before_line_comment(
        "One. Visit https://example.com. // Two.\n",
        1,
        "One.\nVisit https://example.com. // Two.\n"
    )]
    #[case::initial_at_sentence_start_reads_as_list_marker(
        "One. A. Smith wrote it. Next.\n",
        1,
        "One. A. Smith wrote it.\nNext.\n"
    )]
    #[case::initials_inside_sentence(
        "One. Then A. Smith wrote it. Next.\n",
        1,
        "One.\nThen A. Smith wrote it.\nNext.\n"
    )]
    #[case::ordinal_date_at_sentence_start_reads_as_list_marker(
        "Ende. 30. Oktober war es. Dann.\n",
        1,
        "Ende. 30. Oktober war es.\nDann.\n"
    )]
    #[case::ordinal_date_inside_sentence(
        "Ende. Am 30. Oktober war es. Dann.\n",
        1,
        "Ende.\nAm 30. Oktober war es.\nDann.\n"
    )]
    // A line holding only a formatted label ending in `:` stays on its own
    #[case::bold_label_line(
        "*Context:*\nThe server needs a parser. It must be fast.\n",
        1,
        "*Context:*\nThe server needs a parser.\nIt must be fast.\n"
    )]
    #[case::italic_and_unconstrained_label_lines(
        "_Decision:_\nWe use it.\n**Konsequenzen:**\nMore work. Less risk.\n",
        1,
        "_Decision:_\nWe use it.\n**Konsequenzen:**\nMore work.\nLess risk.\n"
    )]
    #[case::label_line_after_text_without_sentence_end(
        "We decided\n*Context:*\nIt was late. Done.\n",
        1,
        "We decided\n*Context:*\nIt was late.\nDone.\n"
    )]
    #[case::plain_line_ending_in_colon(
        "The options are:\nfoo and bar. Next.\n",
        1,
        "The options are: foo and bar.\nNext.\n"
    )]
    #[case::label_with_colon_outside_span(
        "*Context*:\nThe server needs it. Next.\n",
        1,
        "*Context*: The server needs it.\nNext.\n"
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
    // Joining would turn the paragraph's first line into block syntax
    #[case::join_makes_description_list("A\nb:: c.\n", 1)]
    #[case::join_makes_description_list_at_line_end("A\nb::\n", 1)]
    #[case::join_makes_semicolon_description_list("A\nb;; c.\n", 1)]
    #[case::join_makes_nested_description_list("A\nb::: c.\n", 1)]
    #[case::join_makes_deeper_description_list("A\nb:::: c.\n", 1)]
    #[case::join_makes_star_list_item("*\nfoo.\n", 1)]
    #[case::join_makes_dash_list_item("-\nfoo.\n", 1)]
    #[case::join_makes_ordered_list_item("1.\nfoo.\n", 1)]
    #[case::join_makes_dot_list_item(".\nfoo.\n", 1)]
    #[case::join_makes_callout_item("<1>\nfoo.\n", 1)]
    #[case::join_makes_admonition("NOTE:\nfoo.\n", 1)]
    #[case::join_makes_block_macro("image::a.png[x\ny]\n", 1)]
    #[case::join_makes_include_directive("include::a.adoc[x\ny]\n", 1)]
    #[case::join_makes_block_attribute_line("[x\ny]\nfoo.\n", 1)]
    #[case::list_item_read_below_paragraph("One. Two.\n- x. Y.\n", 1)]
    #[case::description_list_read_below_paragraph("One. Two.\nTerm:: d. E.\n", 1)]
    #[case::semicolon_description_list_read_below_paragraph("One. Two.\nTerm;; d. E.\n", 1)]
    fn offers_no_edit(#[case] source: &str, #[case] line: usize) {
        assert_eq!(ventilate_paragraph(source, line).unwrap(), None);
    }
}
