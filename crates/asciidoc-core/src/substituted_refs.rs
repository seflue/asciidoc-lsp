//! Attribute references that Asciidoctor substitutes.

use tree_sitter::{Node, Parser, Range};

use crate::extractors::parse_directive;
use crate::types::{AttributeRef, Directive, PossibleDefinition};
use crate::workspace::entry_name;

/// A byte range of the source in which Asciidoctor substitutes attributes.
struct Region {
    start: usize,
    end: usize,
    /// Text the inline grammar reads: passthroughs inside it are not
    /// substituted. `None` for attribute lists, entry values and macros.
    inline: Option<Range>,
    /// False when only definitions (`{counter:..}`, `{set:..}`) count.
    refs: bool,
}

impl Region {
    fn of(node: Node) -> Self {
        Self {
            start: node.start_byte(),
            end: node.end_byte(),
            inline: None,
            refs: true,
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
    regions.extend(inline_ranges.iter().filter_map(|range| {
        inline_text_scope(root, *range, source).map(|refs| Region {
            start: range.start_byte,
            end: range.end_byte,
            inline: Some(*range),
            refs,
        })
    }));

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
                Found::Reference(_) if !region.refs => {}
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
                .is_some_and(|cell| cell_style(cell, source) == Some('a'));
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => return false,
        }
    }
}

/// Rows of the raw text of an `a` cell that Asciidoctor does not read as
/// text: `//` comments, delimited comment/listing/literal/passthrough blocks
/// and indented literal paragraphs. The grammar leaves the text it does not
/// parse as a block raw in `table_cell_content`; the block structure of
/// that text is recovered line by line.
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
    let mut block_start = true;
    for (offset, line) in source[start..end].split('\n').enumerate() {
        let row = first_row + offset;
        // The first line starts after `a|`: it is never an indented literal.
        let mid_line = offset == 0 && start != line_starts[first_row];
        let blank = line.trim().is_empty();
        let was_open = delimiter.is_some();
        let raw = crate::extractors::is_raw_text_line(line, &mut delimiter);
        // A closing delimiter ends a block; a line comment between blocks
        // keeps the block start, inside a paragraph it does not end it.
        // Asciidoctor reads `//` as a comment only when no third `/` follows.
        let closed = was_open && delimiter.is_none();
        let comment = delimiter.is_none()
            && line
                .strip_prefix("//")
                .is_some_and(|rest| !rest.starts_with('/'));
        let starts_literal = !mid_line
            && block_start
            && !blank
            && delimiter.is_none()
            && line.starts_with([' ', '\t']);
        if raw || (in_literal && !blank) || starts_literal {
            rows.push(row);
        }
        in_literal = !blank && (in_literal || starts_literal);
        block_start = blank || closed || block_start && comment;
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
            regions.extend(
                child_of_kind(node, "line")
                    .filter(|value| entry_value_is_substituted(text(*value, source)))
                    .map(Region::of),
            );
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
                .is_some_and(|subs| subs_has_attributes(subs, false, false));
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
    cols: Option<String>,
}

/// The style, `subs` and `cols` of the attribute lines right above `node`; a
/// later line overrides an earlier one.
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
                    let name = child_of_kind(child, "attribute_name").map(|n| text(n, source));
                    if let (Some(name @ ("subs" | "cols")), Some(value)) =
                        (name, child_of_kind(child, "attribute_value"))
                    {
                        let value = text(value, source)
                            .trim()
                            .trim_matches(['"', '\''])
                            .to_string();
                        if name == "subs" {
                            attrs.subs = Some(value);
                        } else {
                            attrs.cols = Some(value);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    attrs
}

/// Whether the value of an attribute entry gets attribute substitution.
/// Asciidoctor matches the whole value against
/// `AttributeEntryPassMacroRx` (`\Apass:([a-z]+(?:,[a-z-]+)*)?\[(.*)\]\Z`):
/// on a match the content gets only the macro's substitutions, otherwise the
/// header substitutions apply, which include attributes.
fn entry_value_is_substituted(value: &str) -> bool {
    let value = value.trim();
    let Some(rest) = value.strip_prefix("pass:") else {
        return true;
    };
    let Some((spec, _)) = rest.split_once('[') else {
        return true;
    };
    if !value.ends_with(']') {
        return true;
    }
    let mut tokens = spec.split(',');
    let first_ok = tokens.next().is_some_and(|t| {
        (spec.is_empty() || !t.is_empty()) && t.bytes().all(|b| b.is_ascii_lowercase())
    });
    let rest_ok =
        tokens.all(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'));
    if !(first_ok && rest_ok) {
        return true;
    }
    subs_has_attributes(spec, false, true)
}

/// Whether the substitutions `subs` include attributes, starting from the
/// block's default. `+x` / `x+` add, `-x` removes; plain names are
/// collected as a union that replaces the default (Asciidoctor's
/// `resolve_subs`). Block `subs` have no short forms; `short_forms` adds
/// `a` and `n` for the subs of a pass macro.
fn subs_has_attributes(subs: &str, default: bool, short_forms: bool) -> bool {
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
        let carries =
            matches!(name, "attributes" | "normal") || (short_forms && matches!(name, "a" | "n"));
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

/// Whether a delimited block has style `verse` and `subs` without
/// `attributes`. Verse has the normal substitutions by default.
fn verse_without_attributes(block: Node, source: &str) -> bool {
    let attrs = block_attrs(block, source);
    attrs.style.as_deref() == Some("verse")
        && attrs
            .subs
            .as_deref()
            .is_some_and(|subs| !subs_has_attributes(subs, true, false))
}

/// Whether a `cols` value has an `a` or `l` column. The style is the last
/// letter of a column spec.
fn has_verbatim_column(cols: &str) -> bool {
    cols.split([',', ';'])
        .any(|spec| spec.trim().ends_with(['a', 'l']))
}

/// Whether the table around `cell` has an `a` or `l` column.
fn table_has_verbatim_column(cell: Node, source: &str) -> bool {
    let mut node = cell;
    while let Some(parent) = node.parent() {
        if parent.kind() == "table_block" {
            return block_attrs(parent, source)
                .cols
                .as_deref()
                .is_some_and(has_verbatim_column);
        }
        node = parent;
    }
    false
}

/// The style letter of a cell spec, if it has one.
fn cell_style(cell: Node, source: &str) -> Option<char> {
    let attr = child_of_kind(cell, "table_cell_attr")?;
    text(attr, source)
        .chars()
        .last()
        .filter(|style| "adehlms".contains(*style))
}

/// How the text of an inline range is substituted. `None`: not at all (its
/// paragraph or admonition lacks `attributes` among its substitutions, an open
/// block around it takes the text for something else, or it is a literal
/// table cell). `Some(true)`: references and definitions count. `Some(false)`:
/// a cell without a style of its own in a table with an `a` or `l` column,
/// whose references are not checked but whose counters may still define.
/// Compound blocks (example, sidebar, quote) ignore `subs`; a quote or open
/// block with style `verse` is a leaf and obeys them.
fn inline_text_scope(root: Node, range: Range, source: &str) -> Option<bool> {
    let mut node = root.descendant_for_byte_range(range.start_byte, range.end_byte)?;
    let mut refs = true;
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
                    .map_or(default, |subs| subs_has_attributes(subs, default, false));
                if !has {
                    return None;
                }
            }
            "quoted_block" | "open_block" if verse_without_attributes(node, source) => {
                return None;
            }
            "open_block"
                if block_attrs(node, source)
                    .style
                    .as_deref()
                    .is_some_and(|style| VERBATIM_STYLES.contains(&style)) =>
            {
                return None;
            }
            "table_cell" => {
                // The content of an `a` cell is a document of its own: its
                // nested blocks are walked here like top-level ones.
                match cell_style(node, source) {
                    Some('l') => return None,
                    Some(_) => {}
                    None => refs &= !table_has_verbatim_column(node, source),
                }
            }
            _ => {}
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => return Some(refs),
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

/// The reader's view of the conditionals, line by line: the directives
/// [`parse_directive`] takes, a line it rejects is text. Their block form
/// opens, `endif` closes; the single-line form with content opens nothing. Conditionals are
/// not evaluated (the attributes they test depend on the build), so the
/// lines they cover are not checked; an entry in a single-line conditional
/// is only a possible definition.
fn scan_preprocessor(source: &str) -> Preprocessor {
    // How many block conditionals are open.
    let mut open = 0usize;
    let mut locked = Vec::new();
    let mut possible = Vec::new();
    for (index, line) in source.lines().enumerate() {
        match parse_directive(line.trim_end()) {
            Some(Directive::Condition { content, .. }) => {
                locked.push(true);
                match content {
                    None => open += 1,
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
                }
            }
            Some(Directive::Ifeval) => {
                locked.push(true);
                open += 1;
            }
            Some(Directive::Endif) => {
                locked.push(true);
                open = open.saturating_sub(1);
            }
            None => {
                locked.push(open > 0);
                if open > 0 {
                    if let Some(name) = entry_line_name(line) {
                        possible.push(PossibleDefinition {
                            name: name.to_string(),
                            line: index + 1,
                        });
                    }
                }
            }
        }
    }
    Preprocessor { locked, possible }
}

/// Per 0-based line of `source`: a conditional directive (`ifdef`, `ifndef`,
/// `ifeval`, `endif`) or a line inside a block conditional. The LSP does not
/// evaluate conditionals, so it cannot tell whether such a line is rendered.
#[must_use]
pub(crate) fn conditional_lines(source: &str) -> Vec<bool> {
    scan_preprocessor(source).locked
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
    #[case::quote_block_ignores_subs("[quote,subs=-attributes]\n____\n{x}\n____\n")]
    #[case::quote_block_no_style_ignores_subs("[subs=-attributes]\n____\n{x}\n____\n")]
    #[case::verse_block("[verse]\n____\n{x}\n____\n")]
    #[case::verse_block_plus_attributes("[verse,subs=+attributes]\n____\n{x}\n____\n")]
    #[case::verse_open_block("[verse]\n--\n{x}\n--\n")]
    #[case::paragraph("A {x} b\n")]
    #[case::asciidoc_cell_paragraph("|===\na| Para {x}\n|===\n")]
    #[case::asciidoc_cell_list_item("|===\na|\n* item {x}\n|===\n")]
    #[case::entry_pass_attributes_short(":p: pass:a[{x}]\n")]
    #[case::entry_pass_normal_short(":p: pass:n[{x}]\n")]
    #[case::entry_pass_attributes_long(":p: pass:attributes[{x}]\n")]
    #[case::entry_pass_inside_value(":p: pre pass:[{x}] post\n")]
    #[case::entry_pass_trailing_text(":p: pass:[{x}] trail\n")]
    #[case::entry_pass_uppercase_spec(":p: pass:A[{x}]\n")]
    #[case::entry_pass_empty_token(":p: pass:a,[{x}]\n")]
    #[case::entry_pass_empty_first_token(":p: pass:,q[{x}]\n")]
    #[case::entry_pass_minus_first_token(":p: pass:-a[{x}]\n")]
    #[case::entry_pass_plus_first_token(":p: pass:+q[{x}]\n")]
    #[case::entry_pass_plus_later_token(":p: pass:c,+a[{x}]\n")]
    #[case::entry_pass_continued_trailing(":p: pass:[a] \\\n{x}\n")]
    #[case::entry_pass_unclosed(":p: pass:[{x}\n")]
    #[case::cols_cell_spec_overrides("[cols=\"1l,1a\"]\n|===\nd|d1 {x}\n|b\n|===\n")]
    #[case::cols_monospace_column("[cols=\"1m\"]\n|===\n| {x}\n|===\n")]
    #[case::cols_without_style("[cols=\"1,2\"]\n|===\n| {x}\n|b\n|===\n")]
    #[case::cell_indent_after_marker_is_text("|===\na|  ind {x}\n|===\n")]
    #[case::cell_text_after_literal_ends_at_blank("|===\na|\n  lit\n\nafter {x}\n|===\n")]
    #[case::cell_indent_continues_paragraph_after_comment(
        "|===\na|\npara\n// c\n  ind {x}\n|===\n"
    )]
    #[case::cell_indent_after_triple_slash_text("|===\na|\n/// x\n  ind {x}\n|===\n")]
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
    #[case::verse_block_minus_attributes("[verse,subs=-attributes]\n____\n{x}\n____\n")]
    #[case::verse_block_subs_none("[verse,subs=none]\n____\n{x}\n____\n")]
    #[case::verse_block_subs_quotes("[verse,subs=quotes]\n____\n{x}\n____\n")]
    #[case::verse_block_split_lists("[verse]\n[subs=-attributes]\n____\n{x}\n____\n")]
    #[case::verse_block_titled(
        ".T\n[[a]]\n[verse,Author,Cite,subs=-attributes]\n____\n{x}\n____\n"
    )]
    #[case::verse_block_spaced("[verse, Author, Cite, subs=-attributes]\n____\n{x}\n____\n")]
    #[case::verse_open_block_minus_attributes("[verse,subs=-attributes]\n--\n{x}\n--\n")]
    #[case::fenced("```\n{x}\n```\n")]
    #[case::literal_cell("|===\nl|{x}\n|===\n")]
    #[case::listing_in_a_cell("|===\na|\n----\n{x}\n----\n|===\n")]
    #[case::listing("----\n{x}\n----\n")]
    #[case::cell_line_comment("|===\na|\n// c {x}\n|===\n")]
    #[case::cell_line_comment_after_text("|===\na| t\n// c {x}\n|===\n")]
    #[case::cell_block_comment("|===\na|\n////\n{x}\n////\n|===\n")]
    #[case::cell_indented_literal("|===\na| t\n\n  ind {x}\n|===\n")]
    #[case::entry_pass_macro(":p: pass:[{x}]\n")]
    #[case::entry_pass_quotes(":p: pass:q[{x}]\n")]
    #[case::entry_pass_removed(":p: pass:c,-a[{x}]\n")]
    #[case::entry_pass_verbatim(":p: pass:v[{x}]\n")]
    #[case::entry_pass_greedy(":p: pass:[a] {x} pass:[b]\n")]
    #[case::entry_pass_continued(":p: pass:[{x} \\\nmore]\n")]
    #[case::entry_pass_in_body("= T\n\n== S\n\n:p: pass:[{x}]\n")]
    #[case::cols_asciidoc_line_comment("[cols=\"1a,1\"]\n|===\n|\n// c {x}\n| b\n|===\n")]
    #[case::cols_asciidoc_block_comment("[cols=\"1a\"]\n|===\n|\n////\nb {x}\n////\n|===\n")]
    #[case::cols_literal("[cols=\"1l\"]\n|===\n|lit {x}\n|===\n")]
    #[case::cols_literal_nested_list("[cols=\"1l\"]\n|===\n|\n* item {x}\n|===\n")]
    #[case::literal_cell_nested_list("|===\nl|\n* item {x}\n|===\n")]
    #[case::cols_multiplier("[cols=\"2*a\"]\n|===\n|x\n// c {x}\n|y\n|===\n")]
    #[case::cols_aligned_semicolon("[cols=\"<.^2a;1\"]\n|===\n|x\n// c {x}\n|y\n|===\n")]
    #[case::cols_titled_table(".T\n[cols=\"1l\"]\n|===\n|lit {x}\n|===\n")]
    #[case::cols_unknown_cell_style_v("[cols=\"1l\"]\n|===\nv|v1 {x}\n|===\n")]
    #[case::cols_literal_span_cell("[cols=\"1l,1l\"]\n|===\n2+|lit {x}\n|===\n")]
    #[case::cols_asciidoc_listing("[cols=\"1a,1\"]\n|===\n|\n----\ncode {x}\n----\n| b\n|===\n")]
    #[case::cell_literal_after_line_comment("|===\na|\n// c\n  ind {x}\n|===\n")]
    #[case::cell_literal_after_block_comment("|===\na|\n////\nc\n////\n  ind {x}\n|===\n")]
    #[case::cell_literal_after_block_comment_after_text(
        "|===\na| t\n////\nc\n////\n  ind {x}\n|===\n"
    )]
    #[case::cell_literal_after_listing("|===\na|\n----\ncode\n----\n  after {x}\n|===\n")]
    fn quiet_contexts(#[case] source: &str) {
        assert_eq!(names(source), Vec::<String>::new(), "{source:?}");
    }

    /// Asciidoctor warns for a cell of a column without a style next to an `a`
    /// or `l` column. Cells are not mapped to columns (spans would make that
    /// guess wrong), so every unstyled cell of such a table stays unchecked.
    #[test]
    fn table_with_asciidoc_or_literal_column_is_not_checked() {
        assert_eq!(
            names("[cols=\"1,1a\"]\n|===\n| p {x}\n|b\n|===\n"),
            Vec::<String>::new()
        );
    }

    /// The cell is not checked, but a counter in it may still define the
    /// attribute for the rest of the document.
    #[test]
    fn unchecked_cell_keeps_possible_definitions() {
        for source in [
            "[cols=\"1,1a\"]\n|===\n|{counter:step}\n|text\n|===\n\nLast {step}\n",
            "[cols=\"1a\"]\n|===\n|{counter:step}\n|===\n\nLast {step}\n",
        ] {
            let (refs, possible) = extract_str(source);
            assert_eq!(
                refs.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
                vec!["step"],
                "{source:?}"
            );
            assert!(
                possible.iter().any(|p| p.name == "step" && p.line == 3),
                "{source:?}: {possible:?}"
            );
        }
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

    /// The reader takes a directive it rejects for a text line, or drops it
    /// without opening a block: whitespace in the target, text in an `endif`,
    /// no target.
    #[test]
    fn malformed_directives_open_and_close_nothing() {
        assert_eq!(conditional_lines("ifdef::a b[]\nX\n"), [false, false]);
        assert_eq!(conditional_lines("ifdef::[]\nX\n"), [false, false]);
        assert_eq!(
            conditional_lines("ifdef::g[]\nendif::x[text]\nY\nendif::[]\nZ\n"),
            [true, true, true, true, false]
        );
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
        assert_eq!(
            subs_has_attributes(subs, default, false),
            expected,
            "{subs:?}"
        );
    }

    #[rstest]
    #[case("a", true)]
    #[case("n", true)]
    #[case("q,a", true)]
    #[case("a,-a", false)]
    #[case("c,-a", false)]
    #[case("q", false)]
    #[case("", false)]
    fn pass_macro_subs_resolution(#[case] subs: &str, #[case] expected: bool) {
        assert_eq!(subs_has_attributes(subs, false, true), expected, "{subs:?}");
    }
}
