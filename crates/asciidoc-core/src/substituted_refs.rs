//! Attribute references that Asciidoctor substitutes.

use tree_sitter::{Node, Parser, Range};

use crate::types::{AttributeRef, PossibleDefinition};
use crate::workspace::entry_name;

/// A byte range of the source in which Asciidoctor substitutes attributes.
struct Region {
    start: usize,
    end: usize,
    /// Text the inline grammar reads: passthroughs inside it are not
    /// substituted. `None` for attribute lists, entry values and macros.
    inline: Option<Range>,
}

impl Region {
    fn of(node: Node) -> Self {
        Self {
            start: node.start_byte(),
            end: node.end_byte(),
            inline: None,
        }
    }
}

/// The `{name}` references Asciidoctor substitutes, in document order, and
/// the attributes that may be defined without the LSP being able to tell.
///
/// The contexts are a positive list read from the block tree (Asciidoctor
/// substitutes attributes by regex over the text of those contexts, so the
/// names are found the same way). Left out: comments, passthroughs, blocks
/// whose substitutions do not include `attributes`, and every line a
/// preprocessor conditional covers, since the LSP does not evaluate them.
#[must_use]
pub fn extract(
    root: Node,
    source: &str,
    inline_ranges: &[Range],
) -> (Vec<AttributeRef>, Vec<PossibleDefinition>) {
    let preprocessor = scan_preprocessor(source);
    let mut regions = Vec::new();
    collect_block_regions(root, source, &mut regions);
    regions.extend(
        inline_ranges
            .iter()
            .filter(|range| inline_text_is_substituted(root, **range, source))
            .map(|range| Region {
                start: range.start_byte,
                end: range.end_byte,
                inline: Some(*range),
            }),
    );

    let line_starts = line_starts(source);
    let mut inline_parser = crate::parser::init_inline_parser().ok();
    let mut refs = Vec::new();
    let mut possible = preprocessor.possible;
    for region in &regions {
        if !source[region.start..region.end].contains('{') {
            continue;
        }
        let mut passthroughs = Vec::new();
        if let Some(range) = region.inline {
            let Some(parser) = inline_parser.as_mut() else {
                continue;
            };
            collect_passthroughs(parser, range, source, &mut passthroughs);
        }
        let raw_rows = if region
            .inline
            .is_some_and(|range| is_asciidoc_cell(root, range, source))
        {
            asciidoc_cell_raw_rows(source, region.start, region.end, &line_starts)
        } else {
            Vec::new()
        };
        for hit in scan(source, region.start, region.end) {
            let row = line_starts.partition_point(|&start| start <= hit.start) - 1;
            if raw_rows.contains(&row) {
                continue;
            }
            // A counter or `set` on a locked line may still run.
            let locked = preprocessor.locked.get(row).copied().unwrap_or(false)
                && !matches!(hit.found, Found::Possible(_));
            if locked
                || passthroughs
                    .iter()
                    .any(|&(start, end)| hit.start < end && start < hit.end)
            {
                continue;
            }
            match hit.found {
                Found::Reference(name) => refs.push(AttributeRef {
                    name: name.to_string(),
                    line: row + 1,
                    start_col: hit.start - line_starts[row],
                    end_col: hit.end - line_starts[row],
                }),
                Found::Possible(name) => possible.push(PossibleDefinition {
                    name: name.to_string(),
                    line: row + 1,
                }),
                Found::Escaped => {}
            }
        }
    }
    refs.sort_by_key(|reference| (reference.line, reference.start_col));
    refs.dedup_by_key(|reference| (reference.line, reference.start_col));
    possible.sort_by(|a, b| (a.line, &a.name).cmp(&(b.line, &b.name)));
    possible.dedup();
    (refs, possible)
}

/// True when `range` is the content of an `a` cell.
fn is_asciidoc_cell(root: Node, range: Range, source: &str) -> bool {
    let Some(mut node) = root.descendant_for_byte_range(range.start_byte, range.end_byte) else {
        return false;
    };
    loop {
        if node.kind() == "table_cell_content" {
            return node
                .parent()
                .and_then(|cell| child_of_kind(cell, "table_cell_attr"))
                .is_some_and(|attr| text(attr, source).ends_with('a'));
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => return false,
        }
    }
}

/// Rows of the raw text of an `a` cell that Asciidoctor does not read as
/// text: `//` comments, delimited comment/listing/literal/passthrough blocks
/// and indented literal paragraphs. The grammar gives the cell as one raw
/// range, so the nested block structure is recovered line by line.
fn asciidoc_cell_raw_rows(
    source: &str,
    start: usize,
    end: usize,
    line_starts: &[usize],
) -> Vec<usize> {
    let first_row = line_starts.partition_point(|&s| s <= start) - 1;
    let mut rows = Vec::new();
    let mut delimiter = None;
    let mut in_literal = false;
    let mut after_blank = true;
    for (offset, line) in source[start..end].split('\n').enumerate() {
        let row = first_row + offset;
        // The first line starts after `a|`: it is never an indented literal.
        let mid_line = offset == 0 && start != line_starts[first_row];
        let blank = line.trim().is_empty();
        let raw = crate::extractors::is_raw_text_line(line, &mut delimiter);
        let starts_literal = !mid_line
            && after_blank
            && !blank
            && delimiter.is_none()
            && line.starts_with([' ', '\t']);
        if raw || in_literal && !blank || starts_literal {
            rows.push(row);
        }
        in_literal = !blank && (in_literal || starts_literal);
        after_blank = blank;
    }
    rows
}

fn line_starts(source: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(source.match_indices('\n').map(|(index, _)| index + 1))
        .collect()
}

// --- Block-tree regions ---

fn text<'a>(node: Node, source: &'a str) -> &'a str {
    &source[node.start_byte()..node.end_byte()]
}

fn child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    let found = node
        .children(&mut cursor)
        .find(|child| child.kind() == kind);
    found
}

fn collect_block_regions(node: Node, source: &str, regions: &mut Vec<Region>) {
    match node.kind() {
        "document_attr" => {
            regions.extend(child_of_kind(node, "line").map(Region::of));
            return;
        }
        "element_attr" => {
            regions.push(Region::of(node));
            return;
        }
        "block_macro" => {
            let name = child_of_kind(node, "block_macro_name").map_or("", |n| text(n, source));
            let in_raw_body = node
                .parent()
                .is_some_and(|p| matches!(p.kind(), "listing_block_body" | "literal_block_body"));
            let checked = if in_raw_body {
                name == "include"
            } else {
                !matches!(name, "ifdef" | "ifndef" | "ifeval" | "endif")
            };
            if checked {
                regions.push(Region::of(node));
            }
            return;
        }
        "line_comment" | "block_comment" => return,
        "listing_block" | "literal_block" | "passthrough_block" | "ident_block" => {
            let attrs = block_attrs(node, source);
            let has_attributes = attrs
                .subs
                .as_deref()
                .is_some_and(|subs| subs_has_attributes(subs, false));
            if has_attributes {
                let body = if node.kind() == "ident_block" {
                    Some(node)
                } else {
                    let mut cursor = node.walk();
                    let found = node
                        .children(&mut cursor)
                        .find(|child| child.kind().ends_with("_body"));
                    found
                };
                regions.extend(body.map(Region::of));
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_block_regions(child, source, regions);
    }
}

#[derive(Default)]
struct BlockAttrs {
    style: Option<String>,
    subs: Option<String>,
}

/// The style and `subs` of the attribute lines right above `node`; a later
/// line overrides an earlier one.
fn block_attrs(node: Node, source: &str) -> BlockAttrs {
    let mut lists = Vec::new();
    let mut sibling = node.prev_sibling();
    while let Some(previous) = sibling {
        match previous.kind() {
            "element_attr" => lists.push(previous),
            "block_title" | "block_anchor" | "line_comment" | "block_comment" => {}
            _ => break,
        }
        sibling = previous.prev_sibling();
    }
    let mut attrs = BlockAttrs::default();
    for list in lists.into_iter().rev() {
        let mut first_positional = true;
        let mut cursor = list.walk();
        for child in list.children(&mut cursor) {
            match child.kind() {
                "positional_attr" if first_positional => {
                    first_positional = false;
                    if let Some(style) = child_of_kind(child, "block_style") {
                        attrs.style = Some(text(style, source).to_string());
                    }
                }
                "named_attr" => {
                    let is_subs = child_of_kind(child, "attribute_name")
                        .is_some_and(|name| text(name, source) == "subs");
                    if let (true, Some(value)) = (is_subs, child_of_kind(child, "attribute_value"))
                    {
                        attrs.subs = Some(
                            text(value, source)
                                .trim()
                                .trim_matches(['"', '\''])
                                .to_string(),
                        );
                    }
                }
                _ => {}
            }
        }
    }
    attrs
}

/// Whether the substitutions `subs` include attributes, starting from the
/// block's default. `+x` / `x+` add, `-x` removes; plain names are
/// collected as a union that replaces the default (Asciidoctor's
/// `resolve_subs`). Only the full names count: block `subs` have no short
/// forms.
fn subs_has_attributes(subs: &str, default: bool) -> bool {
    let mut state: Option<bool> = None;
    for token in subs.split(',').map(str::trim) {
        let (name, change) = if let Some(name) = token.strip_prefix('+') {
            (name, Some(true))
        } else if let Some(name) = token.strip_suffix('+') {
            (name, Some(true))
        } else if let Some(name) = token.strip_prefix('-') {
            (name, Some(false))
        } else {
            (token, None)
        };
        let carries = matches!(name, "attributes" | "normal");
        match change {
            Some(true) => state = Some(state.unwrap_or(default) || carries),
            Some(false) => state = Some(state.unwrap_or(default) && !carries),
            None => state = Some(state.unwrap_or(false) || carries),
        }
    }
    state.unwrap_or(default)
}

/// Styles that change what a paragraph is: no attribute substitution by
/// default.
const VERBATIM_STYLES: [&str; 8] = [
    "source",
    "listing",
    "literal",
    "pass",
    "stem",
    "latexmath",
    "asciimath",
    "comment",
];

/// Whether the text of an inline range is substituted: its paragraph or
/// admonition has `attributes` among its substitutions, no open block around
/// it takes the text for something else and it is no literal table cell.
/// Compound blocks (example, sidebar, quote) ignore `subs`.
fn inline_text_is_substituted(root: Node, range: Range, source: &str) -> bool {
    let Some(mut node) = root.descendant_for_byte_range(range.start_byte, range.end_byte) else {
        return false;
    };
    let mut leaf_seen = false;
    loop {
        match node.kind() {
            "paragraph" | "admonition" if !leaf_seen => {
                leaf_seen = true;
                let attrs = block_attrs(node, source);
                let default = attrs
                    .style
                    .as_deref()
                    .is_none_or(|style| !VERBATIM_STYLES.contains(&style));
                let has = attrs
                    .subs
                    .as_deref()
                    .map_or(default, |subs| subs_has_attributes(subs, default));
                if !has {
                    return false;
                }
            }
            "open_block"
                if block_attrs(node, source)
                    .style
                    .as_deref()
                    .is_some_and(|style| VERBATIM_STYLES.contains(&style)) =>
            {
                return false;
            }
            "table_cell_content" => {
                // The content of an `a` cell is a document of its own: its
                // nested blocks are walked here like top-level ones.
                let literal = node
                    .parent()
                    .and_then(|cell| child_of_kind(cell, "table_cell_attr"))
                    .is_some_and(|attr| text(attr, source).ends_with('l'));
                if literal {
                    return false;
                }
            }
            _ => {}
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => return true,
        }
    }
}

// --- Passthroughs (inline tree) ---

/// Byte ranges of passthroughs in `range`; footnote content is parsed on its
/// own, as the inline grammar reads it as an attribute list.
fn collect_passthroughs(
    parser: &mut Parser,
    range: Range,
    source: &str,
    out: &mut Vec<(usize, usize)>,
) {
    if parser.set_included_ranges(&[range]).is_err() {
        return;
    }
    let Ok(tree) = crate::parser::parse(parser, source) else {
        return;
    };
    walk_passthroughs(tree.root_node(), source, out);
    for footnote in crate::extractors::footnote_content_ranges(tree.root_node()) {
        collect_passthroughs(parser, footnote, source, out);
    }
}

fn walk_passthroughs(node: Node, source: &str, out: &mut Vec<(usize, usize)>) {
    let span = (node.start_byte(), node.end_byte());
    match node.kind() {
        "passthrough" | "macro_passthrough" | "stem_macro" => {
            out.push(span);
            return;
        }
        // `+text+` in backticks is a literal monospace passthrough.
        "monospace" => {
            let node_text = text(node, source);
            if node_text.starts_with("`+") && node_text.ends_with("+`") {
                out.push(span);
                return;
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_passthroughs(child, source, out);
    }
}

// --- Preprocessor lines ---

struct Preprocessor {
    /// Per line: a directive line or a line inside a block conditional.
    locked: Vec<bool>,
    possible: Vec<PossibleDefinition>,
}

/// The reader's view of the conditionals, line by line: `ifdef`, `ifndef`
/// and `ifeval` at the start of a line. Their block form opens, `endif`
/// closes; the single-line form with content opens nothing. Conditionals are
/// not evaluated (the attributes they test depend on the build), so the
/// lines they cover are not checked; an entry in a single-line conditional
/// is only a possible definition.
fn scan_preprocessor(source: &str) -> Preprocessor {
    let mut depth = 0usize;
    let mut locked = Vec::new();
    let mut possible = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let directive = line
            .strip_prefix("ifdef::")
            .or_else(|| line.strip_prefix("ifndef::"));
        if let Some(rest) = directive {
            locked.push(true);
            let content = rest
                .split_once('[')
                .and_then(|(_, content)| content.trim_end().strip_suffix(']'));
            match content {
                Some("") => depth += 1,
                Some(content) => {
                    let entry = content
                        .strip_prefix(':')
                        .and_then(|entry| entry.split_once(':'));
                    if let Some((name, _)) = entry {
                        possible.push(PossibleDefinition {
                            name: entry_name(name).0.to_string(),
                            line: index + 1,
                        });
                    }
                }
                None => {}
            }
        } else if line.starts_with("ifeval::[") {
            locked.push(true);
            depth += 1;
        } else if line.starts_with("endif::") {
            locked.push(true);
            depth = depth.saturating_sub(1);
        } else {
            locked.push(depth > 0);
            if depth > 0 {
                if let Some(name) = entry_line_name(line) {
                    possible.push(PossibleDefinition {
                        name: name.to_string(),
                        line: index + 1,
                    });
                }
            }
        }
    }
    Preprocessor { locked, possible }
}

/// The attribute name of an entry line (`:name:`, `:name!:`, `:!name:`,
/// optionally followed by a value).
fn entry_line_name(line: &str) -> Option<&str> {
    let (name, rest) = line.strip_prefix(':')?.split_once(':')?;
    let (bare, _) = entry_name(name);
    let is_name = !bare.is_empty()
        && bare
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-');
    (is_name && (rest.is_empty() || rest.starts_with(char::is_whitespace))).then_some(bare)
}

// --- Predefined names ---

/// Attributes Asciidoctor 2.0.26 sets without an entry: the ones it computes
/// per conversion, `DEFAULT_ATTRIBUTES` and `INTRINSIC_ATTRIBUTES`
/// (`lib/asciidoctor.rb`), plus `outdir` and `outfile`, which a conversion
/// to a file sets. Kept apart from the attribute registry, which feeds hover
/// and completion.
const PREDEFINED: &[&str] = &[
    "docname",
    "docfile",
    "docdir",
    "docfilesuffix",
    "doctitle",
    "docdate",
    "doctime",
    "docyear",
    "docdatetime",
    "localdate",
    "localtime",
    "localyear",
    "localdatetime",
    "asciidoctor",
    "asciidoctor-version",
    "backend",
    "basebackend",
    "doctype",
    "filetype",
    "outfilesuffix",
    "outdir",
    "outfile",
    "user-home",
    "max-include-depth",
    "attribute-missing",
    "attribute-undefined",
    "iconsdir",
    "stylesdir",
    "stylesheet",
    "htmlsyntax",
    "copycss",
    "webfonts",
    "iconfont-remote",
    // DEFAULT_ATTRIBUTES
    "appendix-caption",
    "appendix-refsig",
    "caution-caption",
    "chapter-refsig",
    "example-caption",
    "figure-caption",
    "important-caption",
    "last-update-label",
    "note-caption",
    "part-refsig",
    "prewrap",
    "sectids",
    "section-refsig",
    "table-caption",
    "tip-caption",
    "toc-placement",
    "toc-title",
    "untitled-label",
    "version-label",
    "warning-caption",
    // INTRINSIC_ATTRIBUTES
    "startsb",
    "endsb",
    "vbar",
    "caret",
    "asterisk",
    "tilde",
    "plus",
    "backslash",
    "backtick",
    "blank",
    "empty",
    "sp",
    "two-colons",
    "two-semicolons",
    "nbsp",
    "deg",
    "zwsp",
    "quot",
    "apos",
    "lsquo",
    "rsquo",
    "ldquo",
    "rdquo",
    "wj",
    "brvbar",
    "pp",
    "cpp",
    "cxx",
    "amp",
    "lt",
    "gt",
];

/// Names that depend on the build (`backend-pdf`, `safe-mode-name`) or on
/// the environment (`env-github`, ADR adoc-0070): never reported.
const PREDEFINED_PREFIXES: &[&str] = &[
    "backend-",
    "basebackend-",
    "doctype-",
    "filetype-",
    "safe-mode-",
    "env-",
];

/// The header lines Asciidoctor reads author and revision from. The LSP
/// does not model them, so they cannot be told from a defined attribute;
/// with a suffix `_2` for further authors.
const HEADER_ATTRIBUTES: &[&str] = &[
    "author",
    "email",
    "firstname",
    "middlename",
    "lastname",
    "authorinitials",
    "authors",
    "revnumber",
    "revdate",
    "revremark",
];

/// Whether Asciidoctor knows `name` (lower case) without an entry, or the
/// LSP cannot know that it does not.
pub(crate) fn is_predefined(name: &str) -> bool {
    if PREDEFINED.contains(&name) || PREDEFINED_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return true;
    }
    let base = name
        .rsplit_once('_')
        .filter(|(_, number)| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
        .map_or(name, |(base, _)| base);
    HEADER_ATTRIBUTES.contains(&base)
}

// --- Reference scanner ---

enum Found<'a> {
    Reference(&'a str),
    /// `{counter:name}`, `{counter2:name}`, `{set:name:value}`.
    Possible(&'a str),
    /// `{name\}`: Asciidoctor leaves it as written.
    Escaped,
}

struct Hit<'a> {
    start: usize,
    end: usize,
    found: Found<'a>,
}

/// The references in `source[start..end]` with Asciidoctor's
/// `AttributeReferenceRx`; a `\` before the `{` escapes it.
fn scan(source: &str, start: usize, end: usize) -> Vec<Hit<'_>> {
    let bytes = source.as_bytes();
    let mut hits = Vec::new();
    let mut at = start;
    while at < end {
        if bytes[at] != b'{' {
            at += 1;
            continue;
        }
        let Some((found, length)) = parse_reference(&source[at + 1..end]) else {
            at += 1;
            continue;
        };
        let after = at + 1 + length;
        let escaped = at > 0 && bytes[at - 1] == b'\\';
        if !escaped {
            hits.push(Hit {
                start: at,
                end: after,
                found,
            });
        }
        at = after;
    }
    hits
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The reference after a `{`: what it is and how many bytes it spans up to
/// and including the `}`.
fn parse_reference(rest: &str) -> Option<(Found<'_>, usize)> {
    for prefix in ["set:", "counter2:", "counter:"] {
        let Some(after) = rest.strip_prefix(prefix) else {
            continue;
        };
        let close = after.find(['}', '\n'])?;
        if close == 0 || after.as_bytes()[close] != b'}' {
            return None;
        }
        let name = after[..close].split(':').next().unwrap_or_default();
        if name.is_empty() {
            return None;
        }
        return Some((Found::Possible(name), prefix.len() + close + 1));
    }
    let mut end = 0;
    let mut previous_is_word = false;
    for (index, c) in rest.char_indices() {
        if is_word(c) {
            end = index + c.len_utf8();
            previous_is_word = true;
        } else if c == '-'
            && previous_is_word
            && rest[index + 1..].chars().next().is_some_and(is_word)
        {
            previous_is_word = false;
        } else {
            break;
        }
    }
    if end == 0 {
        return None;
    }
    let tail = &rest[end..];
    if tail.starts_with('}') {
        Some((Found::Reference(&rest[..end]), end + 1))
    } else if tail.starts_with("\\}") {
        Some((Found::Escaped, end + 2))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    fn extract_str(source: &str) -> (Vec<AttributeRef>, Vec<PossibleDefinition>) {
        let mut parser = crate::parser::init_block_parser().unwrap();
        let tree = crate::parser::parse(&mut parser, source).unwrap();
        let root = tree.root_node();
        let ranges = crate::extractors::collect_inline_ranges(root, source.as_bytes());
        extract(root, source, &ranges)
    }

    fn names(source: &str) -> Vec<String> {
        extract_str(source)
            .0
            .into_iter()
            .map(|reference| reference.name)
            .collect()
    }

    #[test]
    fn reports_typo_in_paragraph() {
        let (refs, _) = extract_str("= Doc\n\nHello {prodcut} there\n");
        assert_eq!(
            refs,
            vec![AttributeRef {
                name: "prodcut".into(),
                line: 3,
                start_col: 6,
                end_col: 15,
            }]
        );
    }

    #[test]
    fn reports_url_and_link_target() {
        assert_eq!(
            names("See https://example.com/{x}/ and link:{y}[]\n"),
            ["x", "y"]
        );
    }

    #[test]
    fn reports_monospace() {
        assert_eq!(names("Use `{x}` here\n"), ["x"]);
    }

    #[rstest]
    #[case::document_title("= Title {x}\n")]
    #[case::section_title("= T\n\n== Sec {x}\n")]
    #[case::block_title(".Title {x}\npara\n")]
    #[case::attrlist_positional("[source,{x}]\n----\ncode\n----\n")]
    #[case::attrlist_id("[#{x}]\npara\n")]
    #[case::attrlist_quote("[quote, {x}]\npara\n")]
    #[case::attribute_entry(":a: {x}\n")]
    #[case::image_macro("image::{x}.png[]\n")]
    #[case::include_macro("include::{x}/a.adoc[]\n")]
    #[case::table_cell("|===\n| {x}\n|===\n")]
    #[case::list_item("* item {x}\n")]
    #[case::dlist_text("term:: {x}\n")]
    #[case::dlist_term("{x}:: def\n")]
    #[case::footnote("text footnote:[{x}]\n")]
    #[case::admonition("NOTE: {x}\n")]
    #[case::example_ignores_subs("[subs=\"-attributes\"]\n====\n{x}\n====\n")]
    #[case::verse("[verse]\n{x}\n")]
    #[case::paragraph("A {x} b\n")]
    #[case::asciidoc_cell_paragraph("|===\na| Para {x}\n|===\n")]
    #[case::asciidoc_cell_list_item("|===\na|\n* item {x}\n|===\n")]
    fn reports_in_contexts(#[case] source: &str) {
        assert_eq!(names(source), ["x"], "{source:?}");
    }

    #[test]
    fn reports_unicode_and_underscore_names() {
        assert_eq!(names("{präfix} {_x} {a-b}\n"), ["präfix", "_x", "a-b"]);
    }

    #[rstest]
    #[case::attributes_plus("[source,subs=\"attributes+\"]\n----\n{x}\n----\n", true)]
    #[case::plus_attributes("[subs=\"+attributes\"]\n----\n{x}\n----\n", true)]
    #[case::normal("[subs=normal]\n----\n{x}\n----\n", true)]
    #[case::verbatim_attributes("[subs=\"verbatim,attributes\"]\n----\n{x}\n----\n", true)]
    #[case::literal_block("[subs=attributes]\n....\n{x}\n....\n", true)]
    #[case::passthrough_block("[subs=attributes]\n++++\n{x}\n++++\n", true)]
    #[case::source_paragraph("[source,subs=attributes+]\n{x}\n", true)]
    #[case::literal_paragraph("[literal,subs=+attributes]\n{x}\n", true)]
    #[case::indented_literal("[subs=+attributes]\n  {x}\n", true)]
    #[case::plain_list_with_attributes(
        "[source,subs=\"attributes,quotes\"]\n----\n{x}\n----\n",
        true
    )]
    #[case::paragraph_plain_list("[subs=\"attributes,quotes\"]\n{x}\n", true)]
    #[case::no_subs("----\n{x}\n----\n", false)]
    #[case::quotes("[subs=\"quotes\"]\n----\n{x}\n----\n", false)]
    #[case::added_and_removed("[subs=\"attributes+,-attributes\"]\n----\n{x}\n----\n", false)]
    fn listing_subs(#[case] source: &str, #[case] reported: bool) {
        let expected: Vec<String> = if reported { vec!["x".into()] } else { vec![] };
        assert_eq!(names(source), expected, "{source:?}");
    }

    #[rstest]
    #[case::literal_block("....\n{x}\n....\n")]
    #[case::indented_literal("  {x}\n")]
    #[case::passthrough_block("++++\n{x}\n++++\n")]
    #[case::stem_block("[stem]\n++++\n{x}\n++++\n")]
    #[case::block_comment("////\n{x}\n////\n")]
    #[case::line_comment("// {x}\n")]
    #[case::escaped_open("\\{x}\n")]
    #[case::escaped_close("{x\\}\n")]
    #[case::triple_plus("+++{x}+++\n")]
    #[case::single_plus("+{x}+\n")]
    #[case::double_plus("++{x}++\n")]
    #[case::double_dollar("$${x}$$\n")]
    #[case::pass_macro("pass:[{x}]\n")]
    #[case::literal_monospace("`+{x}+`\n")]
    #[case::footnote_passthrough("footnote:[+{x}+]\n")]
    #[case::subs_none("[subs=none]\n{x}\n")]
    #[case::subs_minus_attributes("[subs=-attributes]\n{x}\n")]
    #[case::subs_specialcharacters("[subs=specialcharacters]\n{x}\n")]
    #[case::subs_minus_attributes_admonition("[subs=-attributes]\nNOTE: {x}\n")]
    #[case::style_listing("[listing]\n{x}\n")]
    #[case::style_literal("[literal]\n{x}\n")]
    #[case::style_source("[source]\n{x}\n")]
    #[case::style_pass("[pass]\n{x}\n")]
    #[case::style_comment("[comment]\n{x}\n")]
    #[case::style_stem("[stem]\n{x}\n")]
    #[case::open_comment("[comment]\n--\n{x}\n--\n")]
    #[case::open_pass("[pass]\n--\n{x}\n--\n")]
    #[case::open_source("[source]\n--\n{x}\n--\n")]
    #[case::fenced("```\n{x}\n```\n")]
    #[case::literal_cell("|===\nl|{x}\n|===\n")]
    #[case::listing_in_a_cell("|===\na|\n----\n{x}\n----\n|===\n")]
    #[case::listing("----\n{x}\n----\n")]
    #[case::cell_line_comment("|===\na|\n// c {x}\n|===\n")]
    #[case::cell_line_comment_after_text("|===\na| t\n// c {x}\n|===\n")]
    #[case::cell_block_comment("|===\na|\n////\n{x}\n////\n|===\n")]
    #[case::cell_indented_literal("|===\na| t\n\n  ind {x}\n|===\n")]
    fn quiet_contexts(#[case] source: &str) {
        assert_eq!(names(source), Vec::<String>::new(), "{source:?}");
    }

    #[test]
    fn quiet_for_non_names() {
        assert_eq!(
            names("{t.dot} {x y} {-x} {counter:x} {counter2:y} {set:z:1} {} {x-}\n"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn include_inside_listing_is_checked_image_is_not() {
        assert_eq!(
            names("----\ninclude::{x}/a.adoc[]\nimage::{y}.png[]\n----\n"),
            ["x"]
        );
    }

    #[test]
    fn conditional_lines_and_blocks_are_not_checked() {
        let source = "ifdef::g[]\n{a}\nifdef::h[]\n{b}\nendif::[]\n{c}\nendif::[]\n{d}\n\
                      ifdef::g[{e}]\nifeval::[\"{f}\" == \"y\"]\n{g}\nendif::[]\n{h}\n";
        assert_eq!(names(source), ["d", "h"]);
    }

    #[test]
    fn single_line_conditional_entry_is_possible_definition() {
        let (refs, possible) = extract_str("ifndef::imagesdir[:imagesdir: images]\n\n{x}\n");
        assert_eq!(refs.len(), 1);
        assert_eq!(
            possible,
            vec![PossibleDefinition {
                name: "imagesdir".into(),
                line: 1,
            }]
        );
    }

    #[test]
    fn counter_and_set_are_possible_definitions() {
        let (refs, possible) = extract_str("a {counter:n}\nb {counter2:m} {set:s:1}\n");
        assert_eq!(refs, Vec::new());
        assert_eq!(
            possible,
            vec![
                PossibleDefinition {
                    name: "n".into(),
                    line: 1,
                },
                PossibleDefinition {
                    name: "m".into(),
                    line: 2,
                },
                PossibleDefinition {
                    name: "s".into(),
                    line: 2,
                },
            ]
        );
    }

    #[test]
    fn entries_and_counters_in_block_conditional_are_possible_definitions() {
        let (_, possible) = extract_str(
            "ifdef::g[]\n:a!:\n:!b:\n:c: v\nx {counter:n} {set:s:1}\nendif::[]\n:d: w\n",
        );
        let found: Vec<(&str, usize)> =
            possible.iter().map(|p| (p.name.as_str(), p.line)).collect();
        assert_eq!(found, [("a", 2), ("b", 3), ("c", 4), ("n", 5), ("s", 5)]);
    }

    #[rstest]
    #[case("attributes+", false, true)]
    #[case("+attributes", false, true)]
    #[case("normal", false, true)]
    #[case("verbatim,attributes", false, true)]
    #[case("attributes+,-attributes", false, false)]
    #[case("normal,-attributes", false, false)]
    #[case("quotes", true, false)]
    #[case("none", true, false)]
    #[case("-attributes", true, false)]
    #[case("-attributes", false, false)]
    #[case("specialcharacters", true, false)]
    #[case(" attributes , quotes ", false, true)]
    #[case("attributes,quotes", false, true)]
    #[case("attributes,macros", false, true)]
    #[case("quotes,+attributes", false, true)]
    #[case("-attributes,attributes", true, true)]
    #[case("attributes,-attributes", false, false)]
    #[case("+quotes", true, true)]
    #[case("a", false, false)]
    fn subs_resolution(#[case] subs: &str, #[case] default: bool, #[case] expected: bool) {
        assert_eq!(subs_has_attributes(subs, default), expected, "{subs:?}");
    }
}
