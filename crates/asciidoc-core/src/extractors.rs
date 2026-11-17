use crate::types::{
    Anchor, AttributeDef, AttributeRef, ConditionalDirective, ConditionalKind, ConditionalOperator,
    IncludeMacro, Placement, Section, Span, UnterminatedBlock, Xref,
};

// --- Block-level extractors (work on block parse tree) ---

#[must_use]
pub fn extract_sections(node: tree_sitter::Node, source: &[u8]) -> Vec<Section> {
    let mut results = Vec::new();
    walk_for_sections(node, source, &mut results);
    results
}

#[must_use]
pub fn extract_includes(node: tree_sitter::Node, source: &[u8]) -> Vec<IncludeMacro> {
    let mut results = Vec::new();
    walk_for_includes(node, source, &mut results);
    results
}

#[must_use]
pub fn extract_attributes(node: tree_sitter::Node, source: &[u8]) -> Vec<AttributeDef> {
    let mut results = Vec::new();
    walk_for_attributes(node, source, &mut results);
    results
}

// --- Anchor extraction (works on both parse trees) ---

/// Extract anchors: block anchors (`[[id]]` on a line of its own) from the
/// block tree, inline anchors from the inline tree.
#[must_use]
pub fn extract_anchors(node: tree_sitter::Node, source: &[u8]) -> Vec<Anchor> {
    let mut results = Vec::new();
    walk_for_anchors(node, source, &mut results);
    results
}

// --- Inline-level extractors (work on inline parse tree) ---

#[must_use]
pub fn extract_xrefs(node: tree_sitter::Node, source: &[u8]) -> Vec<Xref> {
    let mut results = Vec::new();
    walk_for_xrefs(node, source, &mut results);
    results
}

// --- Attribute reference extraction (byte-based, no tree-sitter) ---

/// Extract all `{name}` attribute references from source text.
///
/// Scans each line for `{name}` patterns using byte-based scanning.
/// Skips escaped references (`\{name}`), empty braces (`{}`), and
/// names containing whitespace (`{with spaces}`).
#[must_use]
pub fn extract_attribute_refs(source: &str) -> Vec<AttributeRef> {
    let mut results = Vec::new();

    for (line_idx, line_str) in source.lines().enumerate() {
        let bytes = line_str.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'{' {
                // Check for escaped brace: \{
                if i > 0 && bytes[i - 1] == b'\\' {
                    i += 1;
                    continue;
                }

                let brace_start = i;
                let name_start = i + 1;
                let mut end = name_start;
                let mut valid = true;
                while end < bytes.len() && bytes[end] != b'}' {
                    if bytes[end] == b' ' || bytes[end] == b'\t' {
                        valid = false;
                        break;
                    }
                    end += 1;
                }
                if valid && end < bytes.len() && bytes[end] == b'}' && end > name_start {
                    let name = &line_str[name_start..end];
                    results.push(AttributeRef {
                        name: name.to_string(),
                        line: line_idx + 1, // 1-based
                        start_col: brace_start,
                        end_col: end + 1, // exclusive end for LSP half-open ranges
                    });
                    i = end + 1;
                    continue;
                }
            }
            i += 1;
        }
    }

    results
}

// --- Tag directive extraction (byte-based, no tree-sitter) ---

/// Names of the tags a file opens (`tag::name[]`), as Asciidoctor's include
/// preprocessor reads them from the raw lines.
///
/// The prefix does not matter (`# `, `//`, `<!-- `), but no word character may
/// precede `tag::`. After `[]` the line ends or a space follows. Only the
/// first directive of a line counts, and only when it is a start directive:
/// an `end::` ends the search of that line. Markup (comment blocks, listings)
/// and conditionals play no role. Names are returned in file order,
/// duplicates included.
#[must_use]
pub fn extract_tag_directives(source: &str) -> Vec<String> {
    source.lines().filter_map(tag_directive_of_line).collect()
}

/// The tag a raw line opens, see [`extract_tag_directives`].
fn tag_directive_of_line(line: &str) -> Option<String> {
    for (start, _) in line.char_indices() {
        let rest = &line[start..];
        let opens = rest.starts_with("tag::");
        if !opens && !rest.starts_with("end::") {
            continue;
        }
        let after_word = line[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        if !after_word {
            continue;
        }
        let rest = &rest["tag::".len()..];
        let run_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let name = rest[..run_end].strip_suffix("[]").filter(|n| !n.is_empty());
        let terminated = rest[run_end..]
            .chars()
            .next()
            .is_none_or(|c| c == ' ' || c == '\r');
        let Some(name) = name.filter(|_| terminated) else {
            continue;
        };
        return opens.then(|| name.to_string());
    }
    None
}

// --- Conditional directive extraction (byte-based, no tree-sitter) ---

/// Extract `ifdef::name[` and `ifndef::name[` directives from source text.
///
/// Only matches directives at the start of a line (`AsciiDoc` spec requirement).
/// Ignores `endif::`, multi-attribute forms, and `ifeval::`.
#[must_use]
pub fn extract_conditionals(source: &str) -> Vec<ConditionalDirective> {
    let mut results = Vec::new();

    for (line_idx, line) in source.lines().enumerate() {
        let (kind, rest) = if let Some(rest) = line.strip_prefix("ifdef::") {
            (ConditionalKind::Ifdef, rest)
        } else if let Some(rest) = line.strip_prefix("ifndef::") {
            (ConditionalKind::Ifndef, rest)
        } else {
            continue;
        };

        if let Some(bracket_pos) = rest.find('[') {
            let name = &rest[..bracket_pos];
            if name.is_empty() {
                continue;
            }

            let is_valid_part = |p: &str| {
                !p.is_empty()
                    && p.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            };

            let (parts, operator) = if name.contains('+') {
                (
                    name.split('+').collect::<Vec<_>>(),
                    ConditionalOperator::And,
                )
            } else if name.contains(',') {
                (name.split(',').collect::<Vec<_>>(), ConditionalOperator::Or)
            } else {
                (vec![name], ConditionalOperator::Single)
            };

            if parts.iter().all(|p| is_valid_part(p)) {
                results.push(ConditionalDirective {
                    kind,
                    attributes: parts.iter().map(std::string::ToString::to_string).collect(),
                    operator,
                    line: line_idx + 1,
                });
            }
        }
    }

    results
}

// --- Inline range extraction for dual-grammar ---

/// Collect byte ranges of `line` nodes inside paragraphs, titles, etc.
/// These ranges are passed to the inline parser via `set_included_ranges`.
#[must_use]
pub fn collect_inline_ranges(node: tree_sitter::Node, source: &[u8]) -> Vec<tree_sitter::Range> {
    let mut ranges = Vec::new();
    walk_for_inline_ranges(node, source, &mut ranges);
    // A description list item's `line` is collected before its `term`; keep
    // document order for callers and for `set_included_ranges`.
    ranges.sort_by_key(|r| r.start_byte);
    ranges
}

/// Extract xrefs and anchors by parsing each inline range individually.
///
/// Parsing all ranges in a single `set_included_ranges` call can cause
/// cross-range misparses: e.g. a `+` in one paragraph matches a `+` in
/// another, creating a false passthrough node that swallows xref/anchor
/// nodes. Parsing per-range isolates each line's parse context.
#[must_use]
pub fn extract_inline_content_safe(
    ranges: &[tree_sitter::Range],
    source: &str,
) -> (Vec<Xref>, Vec<Anchor>) {
    let mut all_xrefs = Vec::new();
    let mut all_anchors = Vec::new();

    let Ok(mut parser) = crate::parser::init_inline_parser() else {
        return (all_xrefs, all_anchors);
    };

    for range in ranges {
        extract_range(
            &mut parser,
            *range,
            source,
            &mut all_xrefs,
            &mut all_anchors,
        );
    }

    (all_xrefs, all_anchors)
}

/// Parse one range and collect its xrefs and anchors. Footnote content is an
/// opaque attribute list in the inline grammar, so each footnote's text is
/// parsed alone right after the range that contains it.
fn extract_range(
    parser: &mut tree_sitter::Parser,
    range: tree_sitter::Range,
    source: &str,
    xrefs: &mut Vec<Xref>,
    anchors: &mut Vec<Anchor>,
) {
    if parser.set_included_ranges(&[range]).is_err() {
        return;
    }
    let Ok(tree) = crate::parser::parse(parser, source) else {
        return;
    };
    xrefs.extend(extract_xrefs(tree.root_node(), source.as_bytes()));
    anchors.extend(extract_anchors(tree.root_node(), source.as_bytes()));
    for footnote in footnote_content_ranges(tree.root_node()) {
        extract_range(parser, footnote, source, xrefs, anchors);
    }
}

/// Byte ranges of the text between `[` and `]` of every `footnote` node.
///
/// The inline grammar models footnote content as an attribute list, so xrefs
/// inside it are only found by parsing these ranges separately. Empty
/// contents are skipped.
#[must_use]
pub fn footnote_content_ranges(root: tree_sitter::Node) -> Vec<tree_sitter::Range> {
    let mut ranges = Vec::new();
    walk_for_footnote_content(root, &mut ranges);
    ranges
}

fn walk_for_footnote_content(node: tree_sitter::Node, ranges: &mut Vec<tree_sitter::Range>) {
    if node.kind() == "footnote" {
        if let Some(range) = footnote_content_range(node) {
            ranges.push(range);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_footnote_content(child, ranges);
    }
}

/// Content range of one `footnote` node, between its `[` and `]` children.
fn footnote_content_range(footnote: tree_sitter::Node) -> Option<tree_sitter::Range> {
    let mut cursor = footnote.walk();
    let mut open = None;
    let mut close = None;
    for child in footnote.children(&mut cursor) {
        match child.kind() {
            "[" if open.is_none() => open = Some(child),
            "]" => close = Some(child),
            _ => {}
        }
    }
    let (open, close) = (open?, close?);
    if close.start_byte() <= open.end_byte() {
        return None;
    }
    Some(tree_sitter::Range {
        start_byte: open.end_byte(),
        end_byte: close.start_byte(),
        start_point: open.end_position(),
        end_point: close.start_position(),
    })
}

// --- Private walk functions ---

fn walk_for_sections(node: tree_sitter::Node, source: &[u8], results: &mut Vec<Section>) {
    let kind = node.kind();

    // document_title = level 0 (= Title)
    // title1 = level 1 (== Title), title2 = level 2, etc.
    let level = match kind {
        "document_title" => Some(0),
        "title1" => Some(1),
        "title2" => Some(2),
        "title3" => Some(3),
        "title4" => Some(4),
        "title5" => Some(5),
        _ => None,
    };

    if let Some(level) = level {
        // Title text is in the `line` child node
        let text = find_child_text(node, "line", source).unwrap_or_default();
        let (title, inline_anchor) = split_trailing_anchor(text.trim());
        if !title.is_empty() {
            let is_header = kind == "document_title" && header_prefix(node);
            // ID lines above the title beat the one trailing the title.
            let id = section_block_id(node, source).or_else(|| inline_anchor.map(str::to_string));
            results.push(Section {
                level,
                title: title.to_string(),
                line: node.start_position().row + 1, // 1-based
                id,
                is_header,
                style: section_style(node, source),
                placement: placement(node),
            });
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_sections(child, source, results);
    }
}

/// Where `node` stands in the block tree: the first ancestor that is not
/// part of the section structure decides. An `ERROR` is a part the grammar
/// could not parse; any other (delimited block, table cell, list item) holds
/// block content, where Asciidoctor reads no section.
fn placement(node: tree_sitter::Node) -> Placement {
    let mut ancestor = node.parent();
    while let Some(parent) = ancestor {
        match parent.kind() {
            "section" | "section_block" | "block_element" | "document" => {
                ancestor = parent.parent();
            }
            "ERROR" => return Placement::Unparsed,
            _ => return Placement::Block,
        }
    }
    Placement::Body
}

/// The block style (`[appendix]`, `[discrete]`) of an `element_attr`, if its
/// first positional attribute has one.
fn element_attr_style(attr: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut cursor = attr.walk();
    let positional = attr
        .children(&mut cursor)
        .find(|c| c.kind() == "positional_attr")?;
    find_child_text(positional, "block_style", source)
}

/// The block style of the attribute lines above a title; the last line that
/// sets one wins.
fn section_style(title: tree_sitter::Node, source: &[u8]) -> Option<String> {
    lines_above_title(title)
        .into_iter()
        .filter(|line| line.kind() == "element_attr")
        .filter_map(|attr| element_attr_style(attr, source))
        .next_back()
}

/// Whether `id` is an ID Asciidoctor accepts for an inline anchor
/// (`InlineAnchorRx`): a letter, `_` or `:`, then word characters, `-`, `:`
/// or `.`.
#[must_use]
pub fn is_valid_anchor_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_' || c == ':')
        && chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | ':' | '.'))
}

/// Split a title into its text and the ID of an inline `[[id]]` /
/// `[[id,reftext]]` anchor at its end, which gives the section that ID. As in
/// Asciidoctor, that needs a space before `[[` and a valid ID. A valid anchor
/// without the space is no section ID, but its text still drops out of the
/// title the implicit ID is generated from.
fn split_trailing_anchor(title: &str) -> (&str, Option<&str>) {
    let Some(rest) = title.strip_suffix("]]") else {
        return (title, None);
    };
    let Some(open) = rest.rfind("[[") else {
        return (title, None);
    };
    let (head, inner) = (&rest[..open], &rest[open + 2..]);
    let (id, reftext) = inner.split_once(',').unwrap_or((inner, "x"));
    if is_valid_anchor_id(id) && !reftext.trim().is_empty() && !head.ends_with('\\') {
        (head.trim_end(), head.ends_with(' ').then_some(id))
    } else {
        (title, None)
    }
}

/// ID and reftext of an attribute list, as Asciidoctor reads them: the
/// `#id` shorthand of the first (style) slot beats `id=`; the last of each
/// wins. An empty ID counts as none.
#[must_use]
pub fn element_attr_id(node: tree_sitter::Node, source: &[u8]) -> Option<(String, Option<String>)> {
    let mut cursor = node.walk();
    let attrs: Vec<_> = node
        .children(&mut cursor)
        .filter(|c| matches!(c.kind(), "positional_attr" | "named_attr"))
        .collect();
    let shorthand = attrs
        .first()
        .filter(|a| a.kind() == "positional_attr")
        .and_then(|a| {
            let mut c = a.walk();
            let last = a.children(&mut c).filter(|n| n.kind() == "id").last();
            last.and_then(|n| n.utf8_text(source).ok())
        });
    let named = |wanted: &str| {
        attrs
            .iter()
            .filter(|a| {
                a.kind() == "named_attr"
                    && find_child_text(**a, "attribute_name", source).as_deref() == Some(wanted)
            })
            .filter_map(|a| find_child_text(*a, "attribute_value", source))
            .map(|v| v.trim_matches(['"', '\'']).to_string())
            .next_back()
    };
    let id = shorthand.map(str::to_string).or_else(|| named("id"))?;
    if id.is_empty() {
        return None;
    }
    Some((id, named("reftext")))
}

/// The ID and reftext a `[[id]]` anchor or an attribute list (`[#id]`,
/// `[id=...]`) registers, if any.
fn block_line_id(node: tree_sitter::Node, source: &[u8]) -> Option<(String, Option<String>)> {
    match node.kind() {
        "block_anchor" => {
            let id = find_child_text(node, "id", source).filter(|id| !id.is_empty())?;
            Some((id, find_child_text(node, "reftext", source)))
        }
        "element_attr" => element_attr_id(node, source),
        _ => None,
    }
}

/// The siblings of a title node that precede it in its parent: the ID /
/// attribute lines and comments above it. Above `titleN` the parent is the
/// `section`, above a level-0 title the `block_element`.
fn lines_above_title(title: tree_sitter::Node) -> Vec<tree_sitter::Node> {
    let Some(parent) = title.parent() else {
        return Vec::new();
    };
    let mut cursor = parent.walk();
    parent
        .children(&mut cursor)
        .take_while(|child| child.id() != title.id())
        .collect()
}

/// The ID the title node carries: the last `[[id]]` anchor or `[#id]` /
/// `[id=...]` attribute list above the title wins.
fn section_block_id(title: tree_sitter::Node, source: &[u8]) -> Option<String> {
    lines_above_title(title)
        .into_iter()
        .filter_map(|child| block_line_id(child, source))
        .next_back()
        .map(|(id, _)| id)
}

/// Whether a level-0 title is the document header: only comments, attribute
/// entries and ID / attribute lines precede it. Anything else makes it a book
/// part. The lines directly above the title are children of its
/// `block_element`; those further up are blocks of their own.
fn header_prefix(title: tree_sitter::Node) -> bool {
    let Some(block) = title.parent() else {
        return false;
    };
    let lines_ok = lines_above_title(title).iter().all(|line| {
        matches!(
            line.kind(),
            "block_anchor" | "element_attr" | "document_attr" | "line_comment" | "block_comment"
        )
    });
    if !lines_ok {
        return false;
    }
    let mut prev = block.prev_sibling();
    while let Some(node) = prev {
        let Some(inner) = node.child(0) else {
            return false;
        };
        if node.kind() != "block_element"
            || !matches!(
                inner.kind(),
                "document_attr" | "line_comment" | "block_comment"
            )
        {
            return false;
        }
        prev = node.prev_sibling();
    }
    true
}

/// Delimited blocks whose closing delimiter the grammar reports as `MISSING`.
///
/// The grammar closes an open block with a zero-width end marker at the end
/// of the file, a `MISSING` node (for `////` a real token of width zero);
/// the first child is the opener. Only that case is
/// reported. An open `++++` block, an open table and nested blocks with an
/// open inner block come out as `ERROR` trees without delimiter pairing;
/// Asciidoctor warns for them, but guessing the pairing here would risk a
/// wrong diagnostic, so they stay silent.
///
/// Only the innermost unterminated block is reported: an open block inside a
/// container swallows the container's closer, and Asciidoctor then considers
/// the container closed. When both really are open, the outer warning is
/// dropped as well; no diagnostic is better than a false one.
///
/// Known gaps, all per-file effects: a block opened in one file and closed
/// in an included one, a delimiter inside an inactive `ifdef`, and the
/// grammar not knowing setext titles.
#[must_use]
pub fn extract_unterminated_blocks(
    node: tree_sitter::Node,
    source: &[u8],
) -> Vec<UnterminatedBlock> {
    let mut results = Vec::new();
    walk_for_unterminated_blocks(node, source, &mut results);
    results
}

/// `(end marker kind, Asciidoctor context)` of a delimited block node kind.
fn block_context(kind: &str) -> Option<(&'static str, &'static str)> {
    Some(match kind {
        "listing_block" => ("listing_block_end_marker", "listing"),
        "literal_block" => ("literal_block_marker", "literal"),
        "delimited_block" => ("delimited_block_end_marker", "example"),
        "sidebar_block" => ("sidebar_block_end_marker", "sidebar"),
        "quoted_block" => ("quoted_block_end_marker", "quote"),
        "open_block" => ("open_block_marker", "open"),
        "block_comment" => ("block_comment_end_marker", "comment"),
        _ => return None,
    })
}

fn walk_for_unterminated_blocks(
    node: tree_sitter::Node,
    source: &[u8],
    results: &mut Vec<UnterminatedBlock>,
) {
    let mut cursor = node.walk();
    let mut inner = Vec::new();
    for child in node.children(&mut cursor) {
        walk_for_unterminated_blocks(child, source, &mut inner);
    }
    let own = if inner.is_empty() {
        unterminated_block(node, source)
    } else {
        None
    };
    results.extend(own);
    results.append(&mut inner);
}

/// The report for `node` itself, when it is a delimited block with a missing
/// end marker. The delimiter is the leading run of the opener's first
/// character: the opener token may carry trailing blanks or a fence language.
fn unterminated_block(node: tree_sitter::Node, source: &[u8]) -> Option<UnterminatedBlock> {
    let (end_marker, context) = block_context(node.kind())?;
    let mut cursor = node.walk();
    let missing = node
        .children(&mut cursor)
        .any(|c| c.kind() == end_marker && (c.is_missing() || c.byte_range().is_empty()));
    if !missing {
        return None;
    }
    let opener = node.child(0)?;
    let text = opener.utf8_text(source).ok()?;
    let first = text.chars().next()?;
    Some(UnterminatedBlock {
        context,
        delimiter: text.chars().take_while(|&c| c == first).collect(),
        line: opener.start_position().row + 1,
    })
}

fn walk_for_includes(node: tree_sitter::Node, source: &[u8], results: &mut Vec<IncludeMacro>) {
    if node.kind() == "block_macro" {
        let macro_name = find_child_text(node, "block_macro_name", source).unwrap_or_default();
        if macro_name == "include" {
            let target = find_child_node(node, "target");
            let path = target
                .and_then(|target| target.utf8_text(source).ok())
                .unwrap_or_default()
                .to_string();
            let attributes = extract_block_macro_attrs(node, source);
            results.push(IncludeMacro {
                path,
                attributes,
                line: node.start_position().row + 1,
                target_col: target.map_or(0, |target| target.start_position().column),
                placement: placement(node),
            });
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_includes(child, source, results);
    }
}

fn walk_for_attributes(node: tree_sitter::Node, source: &[u8], results: &mut Vec<AttributeDef>) {
    if node.kind() == "document_attr" {
        let name = find_child_text(node, "attr_name", source).unwrap_or_default();
        let value = find_child_text(node, "line", source)
            .unwrap_or_default()
            .trim()
            .to_string();
        if !name.is_empty() {
            results.push(AttributeDef {
                name,
                value,
                line: node.start_position().row + 1,
            });
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_attributes(child, source, results);
    }
}

fn walk_for_anchors(node: tree_sitter::Node, source: &[u8], results: &mut Vec<Anchor>) {
    // Skip backtick code spans — no anchors inside monospace
    if node.kind() == "monospace" {
        return;
    }

    // `id_assignment` (inline tree) carries `id` and an optional `reftext`.
    if node.kind() == "id_assignment" {
        let id = find_child_text(node, "id", source).unwrap_or_default();
        let reftext = find_child_text(node, "reftext", source);
        if !id.is_empty() {
            results.push(Anchor {
                id,
                reftext,
                line: node.start_position().row + 1,
            });
        }
    }

    // `anchor:id[]` / `anchor:id[reftext]` (inline tree) is the macro form.
    if node.kind() == "inline_macro"
        && find_child_text(node, "macro_name", source).as_deref() == Some("anchor")
    {
        let id = find_child_text(node, "target", source).unwrap_or_default();
        if is_valid_anchor_id(&id) {
            results.push(Anchor {
                id,
                reftext: find_child_text(node, "attr", source).filter(|s| !s.is_empty()),
                line: node.start_position().row + 1,
            });
        }
        return;
    }

    // Consecutive `[[id]]` / `[#id]` lines above one block form a run;
    // Asciidoctor registers only the last ID of it (title lines and
    // comments in between do not break the run).
    let mut run_last: Option<Anchor> = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some((id, reftext)) = block_line_id(child, source) {
            run_last = Some(Anchor {
                id,
                reftext,
                line: child.start_position().row + 1,
            });
            continue;
        }
        let in_run = matches!(
            child.kind(),
            "block_anchor" | "element_attr" | "block_title" | "line_comment" | "block_comment"
        );
        if !in_run {
            results.extend(run_last.take());
            walk_for_anchors(child, source, results);
        }
    }
    results.extend(run_last);
}

/// Range of `node`, which may span several lines.
fn node_span(node: tree_sitter::Node) -> Span {
    let (start, end) = (node.start_position(), node.end_position());
    Span {
        start_col: start.column,
        end_line: end.row + 1,
        end_col: end.column,
    }
}

fn walk_for_xrefs(node: tree_sitter::Node, source: &[u8], results: &mut Vec<Xref>) {
    let kind = node.kind();

    // Skip backtick code spans — no xrefs inside monospace
    if kind == "monospace" {
        return;
    }

    // Two forms in inline grammar:
    // 1. inline_macro where macro_name == "xref" → xref:target#anchor[text]
    // 2. xref → <<id,text>>
    if kind == "inline_macro" {
        let macro_name = find_child_text(node, "macro_name", source).unwrap_or_default();
        if macro_name == "xref" {
            let target_node = find_child_node(node, "target");
            let target = target_node
                .and_then(|n| n.utf8_text(source).ok())
                .unwrap_or_default()
                .to_string();
            let display = find_child_text(node, "attr", source);
            results.push(Xref {
                target,
                display_text: display.filter(|s| !s.is_empty()),
                line: node.start_position().row + 1,
                span: Some(node_span(node)),
                target_col: target_node.map_or(0, |n| n.start_position().column),
            });
            return; // Don't recurse into matched macro children
        }
    } else if kind == "xref" {
        // <<id,text>> form — get full text and parse.
        // Filter out keyword tokens: the inline grammar also produces `xref` nodes
        // inside `word > macro_name > xref` when "xref" appears as plain text.
        let text = node.utf8_text(source).unwrap_or_default();
        if text.starts_with("<<") && text.ends_with(">>") {
            let inner = text.trim_start_matches("<<").trim_end_matches(">>");
            let (target, display) = match inner.split_once(',') {
                Some((t, d)) => (t.trim().to_string(), Some(d.trim().to_string())),
                None => (inner.trim().to_string(), None),
            };
            let leading_blanks = inner.len() - inner.trim_start().len();
            results.push(Xref {
                target,
                display_text: display,
                line: node.start_position().row + 1,
                span: Some(node_span(node)),
                target_col: node.start_position().column + 2 + leading_blanks,
            });
        }
        return; // Don't recurse into matched xref children
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_xrefs(child, source, results);
    }
}

fn walk_for_inline_ranges(
    node: tree_sitter::Node,
    source: &[u8],
    ranges: &mut Vec<tree_sitter::Range>,
) {
    // Collect `line` nodes inside paragraphs, titles, admonitions, etc.
    // These contain inline content that needs the inline parser.
    let parent_kind = node.kind();
    let is_inline_container = matches!(
        parent_kind,
        "paragraph"
            | "document_title"
            | "title1"
            | "title2"
            | "title3"
            | "title4"
            | "title5"
            | "unordered_list_item"
            | "ordered_list_item"
            | "checked_list_item"
            | "callout_list_item"
            | "block_title"
            | "quoted_line"
            | "admonition"
            | "quoted_block"
            | "quoted_md_block"
            | "description_list_item"
    );

    if is_inline_container {
        let is_quote = matches!(parent_kind, "quoted_block" | "quoted_md_block");
        let mut raw_delimiter: Option<String> = None;
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() != "line" {
                continue;
            }
            if is_quote && is_raw_quote_line(child, source, &mut raw_delimiter) {
                continue;
            }
            ranges.push(child.range());
        }
    }

    // Text indented right below a description list item (no blank line) is
    // the item's text, but the grammar parses it as an `ident_block`.
    collect_dlist_indented_ranges(node, ranges);

    // Table cells (`|` and `a|`) and description list terms are raw leaf
    // tokens without `line` children, so the whole node is one inline range.
    if matches!(parent_kind, "table_cell_content" | "term") {
        ranges.push(node.range());
    }

    // Still recurse into children for nested structures
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_for_inline_ranges(child, source, ranges);
    }
}

/// Quote content is stored as raw `line` children, so nested raw blocks
/// (listing, literal, passthrough, block comment) and line comments show up
/// as prose. Returns true for lines that Asciidoctor does not read as text:
/// the delimiters themselves, everything between them, and `//` comments.
/// `raw_delimiter` carries the open delimiter across calls.
fn is_raw_quote_line(
    line: tree_sitter::Node,
    source: &[u8],
    raw_delimiter: &mut Option<String>,
) -> bool {
    is_raw_text_line(line.utf8_text(source).unwrap_or_default(), raw_delimiter)
}

/// Text variant of [`is_raw_quote_line`], shared with the substituted
/// reference scan for the raw text of `a` cells.
pub(crate) fn is_raw_text_line(line: &str, raw_delimiter: &mut Option<String>) -> bool {
    let text = line.trim_end();
    if let Some(open) = raw_delimiter {
        if text == open {
            *raw_delimiter = None;
        }
        return true;
    }
    let is_delimiter = text.len() >= 4
        && ['-', '.', '+', '/']
            .iter()
            .any(|&c| text.chars().all(|t| t == c));
    if is_delimiter {
        *raw_delimiter = Some(text.to_string());
        return true;
    }
    text.starts_with("//")
}

/// Ranges of the indented lines that follow a description list (a blank line
/// in between is fine; Asciidoctor still reads them as item text). They are
/// children of the sibling `ident_block`. Indented text after any other block
/// is a literal block and is left alone.
fn collect_dlist_indented_ranges(node: tree_sitter::Node, ranges: &mut Vec<tree_sitter::Range>) {
    let mut cursor = node.walk();
    let mut after_dlist = false;
    for child in node.children(&mut cursor) {
        if let Some(block) = find_kind(child, "ident_block").filter(|_| after_dlist) {
            let mut next_row = None;
            let mut lines = block.walk();
            for line in block.children(&mut lines) {
                let row = line.start_position().row;
                if line.kind() != "ident_block_line" || next_row.is_some_and(|n| n != row) {
                    break;
                }
                next_row = Some(row + 1);
                if let Some(marker) = line.child(0).filter(|c| c.kind() == "ident_marker") {
                    ranges.push(tree_sitter::Range {
                        start_byte: marker.end_byte(),
                        end_byte: line.end_byte(),
                        start_point: marker.end_position(),
                        end_point: line.end_position(),
                    });
                }
            }
        }
        after_dlist = ends_with_kind(child, "description_list");
    }
}

fn ends_with_kind(node: tree_sitter::Node, kind: &str) -> bool {
    if node.kind() == kind {
        return true;
    }
    node.child(node.child_count().saturating_sub(1))
        .is_some_and(|last| ends_with_kind(last, kind))
}

fn find_kind<'a>(node: tree_sitter::Node<'a>, kind: &str) -> Option<tree_sitter::Node<'a>> {
    if node.kind() == kind {
        return Some(node);
    }
    if node.child_count() != 1 {
        return None;
    }
    find_kind(node.child(0)?, kind)
}

// --- Helpers ---

/// Find the first child with the given kind and return its text.
fn find_child_node<'a>(node: tree_sitter::Node<'a>, kind: &str) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    let found = node
        .children(&mut cursor)
        .find(|child| child.kind() == kind);
    found
}

pub fn find_child_text(node: tree_sitter::Node, kind: &str, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return child
                .utf8_text(source)
                .ok()
                .map(std::string::ToString::to_string);
        }
    }
    None
}

/// Extract attributes from `block_macro_attr` children of a `block_macro` node.
/// Each `block_macro_attr` has `attribute_name` and `attribute_value` children.
fn extract_block_macro_attrs(node: tree_sitter::Node, source: &[u8]) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "block_macro_attr" {
            let name = find_child_text(child, "attribute_name", source).unwrap_or_default();
            let value = find_child_text(child, "attribute_value", source).unwrap_or_default();
            if !name.is_empty() {
                attrs.push((name, value));
            }
        }
    }
    attrs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    #[test]
    fn test_extract_sections() {
        let mut p = parser::init_block_parser().unwrap();
        let source =
            "= Document Title\n\n== Chapter One\n\nSome text.\n\n=== Subsection\n\nMore text.\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let sections = extract_sections(tree.root_node(), source.as_bytes());

        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].level, 0);
        assert_eq!(sections[0].title, "Document Title");
        assert_eq!(sections[0].line, 1);
        assert_eq!(sections[1].level, 1);
        assert_eq!(sections[1].title, "Chapter One");
        assert_eq!(sections[2].level, 2);
        assert_eq!(sections[2].title, "Subsection");
    }

    #[test]
    fn test_extract_sections_inline_anchor_in_title() {
        let mut p = parser::init_block_parser().unwrap();
        let source = "= Doc\n\n== Inline Anchor [[tid]]\n\n== Escaped \\[[esc]]\n\n== Bad Anchor [[1bad]]\n\n== NoSpace[[nsp]]\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let sections = extract_sections(tree.root_node(), source.as_bytes());

        assert_eq!(sections[1].title, "Inline Anchor");
        assert_eq!(sections[1].id.as_deref(), Some("tid"));
        for section in &sections[2..] {
            assert_eq!(section.id, None, "{}", section.title);
        }
        assert_eq!(sections[4].title, "NoSpace");
    }

    #[test]
    fn test_extract_sections_inline_anchor_reftext() {
        let mut p = parser::init_block_parser().unwrap();
        let source = "= Doc\n\n== Foo [[tid,]]\n\n== Bar [[bid, Bar Text]]\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let sections = extract_sections(tree.root_node(), source.as_bytes());

        // Asciidoctor needs a non-empty reftext after the comma.
        assert_eq!(sections[1].id, None);
        assert_eq!(sections[2].id.as_deref(), Some("bid"));
        assert_eq!(sections[2].title, "Bar");
    }

    fn anchors_in(source: &str) -> Vec<Anchor> {
        let mut p = parser::init_block_parser().unwrap();
        let tree = parser::parse(&mut p, source).unwrap();
        let ranges = collect_inline_ranges(tree.root_node(), source.as_bytes());
        let (_, anchors) = extract_inline_content_safe(&ranges, source);
        anchors
    }

    #[test]
    fn test_extract_inline_anchor_macro() {
        let source = "A anchor:a1[] b anchor:a2[Some Text] c anchor:[] d \\anchor:esc[] e anchor:1d[] f anchor:sp ace[]\n";
        let got: Vec<_> = anchors_in(source)
            .into_iter()
            .map(|a| (a.id, a.reftext))
            .collect();
        assert_eq!(
            got,
            [
                ("a1".to_string(), None),
                ("a2".to_string(), Some("Some Text".to_string()))
            ]
        );
    }

    #[test]
    fn test_extract_sections_header_with_attribute_entry_between_lines_and_title() {
        // Attribute entries between ID / attribute lines and the title stay
        // part of the header run, as in Asciidoctor.
        let mut p = parser::init_block_parser().unwrap();
        for (source, id) in [
            ("[.role]\n:a: b\n= T\n", None),
            ("[[x]]\n:a: b\n= T\n", Some("x")),
            ("[[x]]\n:a: b\n[[y]]\n= T\n", Some("y")),
            ("[[x]]\n:a: b\n:c: d\n[[y]]\n:e: f\n= T\n", Some("y")),
            ("[[x]]\n:a: b\n[.role]\n= T\n", Some("x")),
        ] {
            let tree = parser::parse(&mut p, source).unwrap();
            let sections = extract_sections(tree.root_node(), source.as_bytes());
            assert!(sections[0].is_header, "{source:?}");
            assert_eq!(sections[0].id.as_deref(), id, "{source:?}");
        }
        // A real paragraph between the lines and the title ends the header.
        let source = "[[x]]\nText.\n:a: b\n\n= T\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let sections = extract_sections(tree.root_node(), source.as_bytes());
        assert!(!sections[0].is_header, "{source:?}");
    }

    #[test]
    fn test_extract_sections_header_title_flag() {
        let mut p = parser::init_block_parser().unwrap();
        let header_forms = [
            "= T\n\n== A\n",
            "// c\n:a: b\n= T\n",
            "[[cid]]\n= T\n",
            "[#cid]\n= T\n",
            ":a: b\n[id=cid]\n= T\n",
        ];
        for source in header_forms {
            let tree = parser::parse(&mut p, source).unwrap();
            let sections = extract_sections(tree.root_node(), source.as_bytes());
            assert!(sections[0].is_header, "{source:?}");
        }
        let source = ":doctype: book\n\nText.\n\n= Part\n\n== C\n\n= Part Two\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let sections = extract_sections(tree.root_node(), source.as_bytes());
        assert!(sections.iter().all(|s| !s.is_header));
    }

    #[test]
    fn test_extract_sections_block_id_above_document_title() {
        let mut p = parser::init_block_parser().unwrap();
        for (source, id) in [
            ("[[cid]]\n= T\n", Some("cid")),
            ("[#cid]\n= T\n", Some("cid")),
            (":a: b\n[id=cid]\n= T\n", Some("cid")),
            // Asciidoctor: the last ID line above the title wins.
            ("[[x]]\n[#y]\n= T\n", Some("y")),
            ("[.role]\n= T\n", None),
            ("= T\n", None),
        ] {
            let tree = parser::parse(&mut p, source).unwrap();
            let sections = extract_sections(tree.root_node(), source.as_bytes());
            assert_eq!(sections[0].id.as_deref(), id, "{source:?}");
        }
    }

    #[rstest::rstest]
    #[case::block_anchor("[[intro]]\n== Introduction\n", Some("intro"))]
    #[case::shorthand("[#aud]\n== A\n", Some("aud"))]
    #[case::id_attribute("[id=by-attr]\n== B\n", Some("by-attr"))]
    #[case::last_id_line_wins("[[x]]\n[#y]\n== S\n", Some("y"))]
    #[case::block_id_beats_inline("[[a]]\n== Both Anchors [[b]]\n", Some("a"))]
    #[case::inline("== Inline Anchor [[tid]]\n", Some("tid"))]
    #[case::inline_without_space("== NoSpace[[nsp]]\n", None)]
    #[case::inline_empty_reftext("== Foo [[tid,]]\n", None)]
    #[case::inline_escaped("== Escaped \\[[esc]]\n", None)]
    #[case::role_only("[.role]\n== R\n", None)]
    #[case::plain("== Options\n", None)]
    #[case::paragraph_anchor_after_title("== Outer\n\n[[para-id]]\nA paragraph.\n", None)]
    fn test_extract_sections_id(#[case] body: &str, #[case] id: Option<&str>) {
        let mut p = parser::init_block_parser().unwrap();
        let source = format!("= Doc\n\n{body}");
        let tree = parser::parse(&mut p, &source).unwrap();
        let sections = extract_sections(tree.root_node(), source.as_bytes());
        assert_eq!(sections[1].id.as_deref(), id, "{source:?}");
    }

    fn styles_of(source: &str) -> Vec<Option<String>> {
        let mut p = parser::init_block_parser().unwrap();
        let tree = parser::parse(&mut p, source).unwrap();
        extract_sections(tree.root_node(), source.as_bytes())
            .into_iter()
            .map(|s| s.style)
            .collect()
    }

    /// Asciidoctor 2.0.26: the last style line above a title wins
    /// (`[glossary]` then `[appendix]` is an appendix).
    #[test]
    fn test_section_style_is_the_block_style_of_its_attribute_lines() {
        let source =
            "= Doc\n\n[glossary#gid.role]\n== A\n\n[#id]\n== B\n\n[glossary]\n[appendix]\n== C\n";
        assert_eq!(
            styles_of(source),
            [
                None,
                Some("glossary".to_string()),
                None,
                Some("appendix".to_string())
            ]
        );
    }

    #[test]
    fn test_discrete_heading_keeps_its_style() {
        let mut p = parser::init_block_parser().unwrap();
        let source = "= Doc\n\n== A\n\n[discrete]\n===== D\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let sections = extract_sections(tree.root_node(), source.as_bytes());
        assert_eq!(sections[2].style.as_deref(), Some("discrete"));
        assert_eq!(sections[2].placement, Placement::Body);
    }

    #[test]
    fn test_level_0_title_takes_the_style_above_it() {
        assert_eq!(
            styles_of("[glossary]\n= Glossary\n"),
            [Some("glossary".to_string())]
        );
    }

    #[test]
    fn test_titles_inside_blocks_are_placed_in_block() {
        let mut p = parser::init_block_parser().unwrap();
        let source = "= Doc\n\n====\n==== Ex\n====\n\n****\n===== Sb\n****\n\n--\n==== Op\n--\n\n|===\na|\n==== Cell\n|===\n\n== Real\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let placements: Vec<_> = extract_sections(tree.root_node(), source.as_bytes())
            .into_iter()
            .map(|s| (s.title, s.placement))
            .collect();
        let expected = [
            ("Doc", Placement::Body),
            ("Ex", Placement::Block),
            ("Sb", Placement::Block),
            ("Op", Placement::Block),
            ("Cell", Placement::Block),
            ("Real", Placement::Body),
        ];
        let expected: Vec<_> = expected.iter().map(|(t, p)| (t.to_string(), *p)).collect();
        assert_eq!(placements, expected);
    }

    #[test]
    fn test_include_inside_listing_and_example_block_is_placed_in_block() {
        let mut p = parser::init_block_parser().unwrap();
        let source = "= Doc\n\n----\ninclude::a.adoc[]\n----\n\n====\ninclude::b.adoc[]\n====\n\ninclude::c.adoc[]\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let placements: Vec<_> = extract_includes(tree.root_node(), source.as_bytes())
            .into_iter()
            .map(|i| (i.path, i.placement))
            .collect();
        let expected = [
            ("a.adoc", Placement::Block),
            ("b.adoc", Placement::Block),
            ("c.adoc", Placement::Body),
        ];
        let expected: Vec<_> = expected.iter().map(|(t, p)| (t.to_string(), *p)).collect();
        assert_eq!(placements, expected);
    }

    #[test]
    fn test_extract_includes() {
        let mut p = parser::init_block_parser().unwrap();
        let source = "= Title\n\ninclude::chapters/intro.adoc[]\n\ninclude::chapters/main.adoc[leveloffset=+1]\n\nSome text.\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let includes = extract_includes(tree.root_node(), source.as_bytes());

        assert_eq!(includes.len(), 2);
        assert_eq!(includes[0].path, "chapters/intro.adoc");
        assert_eq!(includes[0].attributes.len(), 0);
        assert_eq!(includes[1].path, "chapters/main.adoc");
        assert_eq!(includes[1].attributes.len(), 1);
        assert_eq!(
            includes[1].attributes[0],
            ("leveloffset".to_string(), "+1".to_string())
        );
    }

    #[test]
    fn test_extract_attributes() {
        let mut p = parser::init_block_parser().unwrap();
        let source = "= Title\n:imagesdir: images\n:source-dir: ../src\n\nContent.\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let attrs = extract_attributes(tree.root_node(), source.as_bytes());

        assert_eq!(attrs.len(), 2);
        assert_eq!(attrs[0].name, "imagesdir");
        assert_eq!(attrs[0].value, "images");
        assert_eq!(attrs[1].name, "source-dir");
        assert_eq!(attrs[1].value, "../src");
    }

    #[test]
    fn test_extract_xrefs_inline() {
        let mut p = parser::init_inline_parser().unwrap();
        let source = "See xref:file.adoc#anchor[Link Text] for details.";
        let tree = parser::parse(&mut p, source).unwrap();
        println!("Inline xref S-expr: {}", tree.root_node().to_sexp());
        let xrefs = extract_xrefs(tree.root_node(), source.as_bytes());

        assert_eq!(xrefs.len(), 1);
        assert_eq!(xrefs[0].target, "file.adoc#anchor");
    }

    #[test]
    fn test_extract_anchors_inline() {
        let mut p = parser::init_inline_parser().unwrap();
        let source = "[[my-anchor]]";
        let tree = parser::parse(&mut p, source).unwrap();
        println!("Anchor S-expr: {}", tree.root_node().to_sexp());
        let anchors = extract_anchors(tree.root_node(), source.as_bytes());

        // Print result regardless — helps debug if grammar doesn't parse anchors as expected
        println!("Anchors found: {:?}", anchors);
        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].id, "my-anchor");
        assert_eq!(anchors[0].reftext, None);
    }

    #[test]
    fn test_extract_anchor_with_reftext() {
        let mut p = parser::init_inline_parser().unwrap();
        let source = "[[my-anchor,Display Text]]";
        let tree = parser::parse(&mut p, source).unwrap();
        let anchors = extract_anchors(tree.root_node(), source.as_bytes());

        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].id, "my-anchor");
        assert_eq!(anchors[0].reftext, Some("Display Text".to_string()));
    }

    #[test]
    fn test_extract_block_anchors() {
        let mut p = parser::init_block_parser().unwrap();
        let source = "== A\n\nBody.\n\n[[b,Section B]]\n== B\n\n[[para]]\nText.\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let anchors = extract_anchors(tree.root_node(), source.as_bytes());

        assert_eq!(anchors.len(), 2);
        assert_eq!(anchors[0].id, "b");
        assert_eq!(anchors[0].reftext, Some("Section B".to_string()));
        assert_eq!(anchors[0].line, 5);
        assert_eq!(anchors[1].id, "para");
        assert_eq!(anchors[1].reftext, None);
        assert_eq!(anchors[1].line, 8);
    }

    /// Block IDs from attribute lists, as Asciidoctor reads them (oracle:
    /// `tests/fixtures/block_id_attrs/index.adoc`). Each case lists the
    /// `(id, reftext, line)` triples that must be registered.
    #[rstest::rstest]
    #[case::section_shorthand("[#sid]\n== S\n", &[("sid", None, 1)])]
    #[case::shorthand_with_role("[#sid.role]\n== S\n", &[("sid", None, 1)])]
    #[case::role_before_shorthand("[.role#sid]\n== S\n", &[("sid", None, 1)])]
    #[case::shorthand_with_option("[#sid%collapsible]\n== S\n", &[("sid", None, 1)])]
    #[case::named_id("[id=sid]\n== S\n", &[("sid", None, 1)])]
    #[case::quoted_named_id("Intro.\n\n[id=\"pid\"]\nPara.\n", &[("pid", None, 3)])]
    #[case::style_with_shorthand("[source#cid,rust]\n----\ncode\n----\n", &[("cid", None, 1)])]
    #[case::named_id_in_third_slot("[source,rust,id=cid]\n----\ncode\n----\n", &[("cid", None, 1)])]
    #[case::admonition_style("[NOTE#nid]\nText.\n", &[("nid", None, 1)])]
    #[case::shorthand_beats_named("[#a,id=b]\nText.\n", &[("a", None, 1)])]
    #[case::last_named_id_wins("[id=a,id=b]\nText.\n", &[("b", None, 1)])]
    #[case::last_shorthand_wins("[#a.r1#b]\nText.\n", &[("b", None, 1)])]
    #[case::reftext("[#sid,reftext=\"Ref Text\"]\nText.\n", &[("sid", Some("Ref Text"), 1)])]
    #[case::id_in_second_slot_is_no_id("[source,#x]\n----\ncode\n----\n", &[])]
    #[case::role_only("[.role]\nText.\n", &[])]
    #[case::empty_named_id("[id=]\nText.\n", &[])]
    #[case::attr_then_attr("[#a]\n[#b]\nText.\n", &[("b", None, 2)])]
    #[case::anchor_then_attr("[[a]]\n[#b]\nText.\n", &[("b", None, 2)])]
    #[case::attr_then_anchor("[#a]\n[[b]]\nText.\n", &[("b", None, 2)])]
    #[case::anchor_then_anchor("[[a]]\n[[b]]\nText.\n", &[("b", None, 2)])]
    #[case::id_survives_idless_line("[#a]\n[.role]\nText.\n", &[("a", None, 1)])]
    #[case::title_between(
        "[#a]\n.Title\n[#b]\nText.\n",
        &[("b", None, 3)]
    )]
    #[case::separate_blocks(
        "[#a]\nOne.\n\n[#b]\nTwo.\n",
        &[("a", None, 1), ("b", None, 4)]
    )]
    fn test_extract_block_ids_from_attribute_lists(
        #[case] source: &str,
        #[case] expected: &[(&str, Option<&str>, usize)],
    ) {
        let mut p = parser::init_block_parser().unwrap();
        let tree = parser::parse(&mut p, source).unwrap();
        let anchors = extract_anchors(tree.root_node(), source.as_bytes());
        let got: Vec<(&str, Option<&str>, usize)> = anchors
            .iter()
            .map(|a| (a.id.as_str(), a.reftext.as_deref(), a.line))
            .collect();
        assert_eq!(got, expected, "{source:?}");
    }

    #[test]
    fn test_anchor_reftext_xref_e2e() {
        let mut p = parser::init_inline_parser().unwrap();

        // Anchor with reftext
        let anchor_source = "[[my-anchor,Display Text]]";
        let tree = parser::parse(&mut p, anchor_source).unwrap();
        let anchors = extract_anchors(tree.root_node(), anchor_source.as_bytes());

        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].id, "my-anchor");
        assert_eq!(anchors[0].reftext, Some("Display Text".to_string()));

        // Xref targeting that anchor
        let xref_source = "See <<my-anchor>>";
        let tree = parser::parse(&mut p, xref_source).unwrap();
        let xrefs = extract_xrefs(tree.root_node(), xref_source.as_bytes());

        assert_eq!(xrefs.len(), 1);
        assert_eq!(xrefs[0].target, "my-anchor");
        // Xref target matches anchor ID — gd works
        assert_eq!(xrefs[0].target, anchors[0].id);
    }

    #[test]
    fn test_collect_inline_ranges() {
        let mut p = parser::init_block_parser().unwrap();
        let source = "= Title\n\nA paragraph with content.\n\n== Section\n\nAnother paragraph.\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let ranges = collect_inline_ranges(tree.root_node(), source.as_bytes());

        assert!(
            !ranges.is_empty(),
            "Should find inline ranges in paragraphs and titles"
        );
        println!("Found {} inline ranges", ranges.len());
    }

    #[test]
    fn test_collect_inline_ranges_includes_list_items() {
        let mut p = parser::init_block_parser().unwrap();
        let source = "= Title\n\n* Item with <<anchor>>\n* Another item\n. Ordered item\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let ranges = collect_inline_ranges(tree.root_node(), source.as_bytes());

        // List items contain `line` nodes with inline content
        assert!(
            ranges.len() >= 3,
            "Should find inline ranges in list items, got {}",
            ranges.len()
        );
    }

    // --- xrefs in table cells ---

    fn xrefs_in(source: &str) -> Vec<Xref> {
        let mut p = parser::init_block_parser().unwrap();
        let tree = parser::parse(&mut p, source).unwrap();
        let ranges = collect_inline_ranges(tree.root_node(), source.as_bytes());
        let (xrefs, _) = extract_inline_content_safe(&ranges, source);
        xrefs
    }

    #[rstest::rstest]
    #[case::default_cell("= T\n\n|===\n| xref:target.adoc[Label]\n|===\n")]
    #[case::asciidoc_cell("= T\n\n|===\na| xref:target.adoc[Label]\n|===\n")]
    #[case::second_cell_in_row("= T\n\n|===\n| A | xref:target.adoc[Label]\n|===\n")]
    #[case::header_cell(
        "= T\n\n[options=\"header\"]\n|===\n| xref:target.adoc[Label]\n| Other\n|===\n"
    )]
    #[case::multiline_cell("= T\n\n|===\n| first line\nxref:target.adoc[Label]\n|===\n")]
    #[case::escaped_pipe_in_label("= T\n\n|===\n| xref:target.adoc[A \\| B]\n|===\n")]
    fn test_xref_in_table_cell(#[case] source: &str) {
        let xrefs = xrefs_in(source);
        assert_eq!(xrefs.len(), 1, "got {xrefs:?}");
        assert_eq!(xrefs[0].target, "target.adoc");
    }

    #[rstest::rstest]
    #[case::admonition_paragraph("= T\n\nTIP: see <<x>>.\n")]
    #[case::quote_block("= T\n\n[quote]\n____\nsee <<x>>.\n____\n")]
    #[case::quote_block_unstyled("= T\n\n____\nsee <<x>>.\n____\n")]
    #[case::nested_quote_block("= T\n\n____\nouter\n_____\nsee <<x>>.\n_____\n____\n")]
    #[case::markdown_quote("= T\n\n> see <<x>>.\n")]
    #[case::dlist_inline("= T\n\nTerm:: see <<x>>.\n")]
    #[case::dlist_next_line("= T\n\nTerm::\nsee <<x>>.\n")]
    #[case::dlist_term("= T\n\nSee <<x>>:: def\n")]
    #[case::dlist_xref_macro("= T\n\nTerm:: see xref:x.adoc#x[X]\n")]
    #[case::dlist_continuation("= T\n\nTerm:: text\n+\nsee <<x>>.\n")]
    #[case::dlist_indented_body("= T\n\nTerm::\n  indented <<x>>.\n")]
    #[case::dlist_indented_after_blank("= T\n\nTerm::\n\n  indented <<x>>.\n")]
    #[case::dlist_nested_indented("= T\n\nOuter::\n  Inner::: see <<x>>.\n")]
    #[case::footnote("= T\n\nText.footnote:[See <<x>>.]\n")]
    #[case::footnote_with_id("= T\n\nText.footnote:fid[See <<x>>.]\n")]
    #[case::footnote_reftext_with_comma("= T\n\nText.footnote:[See <<x,a, b>>.]\n")]
    fn test_xref_in_block_context(#[case] source: &str) {
        let xrefs = xrefs_in(source);
        assert_eq!(xrefs.len(), 1, "got {xrefs:?}");
        assert!(xrefs[0].target.ends_with('x'), "got {xrefs:?}");
    }

    /// Raw content nested in a quote is not prose: Asciidoctor reports no
    /// xref there.
    #[rstest::rstest]
    #[case::listing("= T\n\n____\n----\nsee <<x>>\n----\n____\n")]
    #[case::source_listing("= T\n\n____\n[source]\n----\nsee <<x>>\n----\n____\n")]
    #[case::literal("= T\n\n____\n....\nsee <<x>>\n....\n____\n")]
    #[case::passthrough("= T\n\n____\n++++\nsee <<x>>\n++++\n____\n")]
    #[case::block_comment("= T\n\n____\n////\nsee <<x>>\n////\n____\n")]
    #[case::line_comment("= T\n\n____\n// see <<x>>\n____\n")]
    #[case::markdown_line_comment("= T\n\n> // see <<x>>\n")]
    #[case::indented_paragraph_without_dlist("= T\n\nText\n\n  see <<x>>.\n")]
    fn test_no_xref_in_raw_content(#[case] source: &str) {
        let xrefs = xrefs_in(source);
        assert!(xrefs.is_empty(), "got {xrefs:?}");
    }

    #[test]
    fn test_xrefs_come_in_document_order() {
        let xrefs = xrefs_in(
            "= T\n\nSee <<a>>.\n\nTerm:: <<b>>\n\nText.footnote:[<<c>>] and <<d>>.\n\nLast <<e>>.\n",
        );
        let targets: Vec<&str> = xrefs.iter().map(|x| x.target.as_str()).collect();
        assert_eq!(targets, ["a", "b", "d", "c", "e"]);
    }

    #[test]
    fn test_escaped_pipe_keeps_cell_boundary_and_raw_label() {
        // The grammar does not split the cell at `\|`. The label keeps the
        // source text; Asciidoctor would render `A | B`. Only the CLI shows
        // labels, so the backslash is not unescaped here.
        let xrefs = xrefs_in("= T\n\n|===\n| xref:target.adoc[A \\| B] | next\n|===\n");
        assert_eq!(xrefs.len(), 1, "got {xrefs:?}");
        assert_eq!(xrefs[0].display_text.as_deref(), Some("A \\| B"));
    }

    #[test]
    fn test_shorthand_xref_in_default_table_cell() {
        let xrefs = xrefs_in("= T\n\n|===\n| See <<my-anchor,Label>>\n|===\n");
        assert_eq!(xrefs.len(), 1, "got {xrefs:?}");
        assert_eq!(xrefs[0].target, "my-anchor");
    }

    // --- Include-Attribute extraction ---

    #[test]
    fn test_extract_include_single_attribute() {
        let mut p = parser::init_block_parser().unwrap();
        let source = "= Title\n\ninclude::chapters/main.adoc[leveloffset=+1]\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let includes = extract_includes(tree.root_node(), source.as_bytes());

        assert_eq!(includes.len(), 1);
        assert_eq!(includes[0].path, "chapters/main.adoc");
        assert_eq!(includes[0].attributes.len(), 1);
        assert_eq!(
            includes[0].attributes[0],
            ("leveloffset".to_string(), "+1".to_string())
        );
    }

    #[test]
    fn test_extract_include_multiple_attributes() {
        let mut p = parser::init_block_parser().unwrap();
        let source = "= Title\n\ninclude::file.adoc[leveloffset=+1,tags=main]\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let includes = extract_includes(tree.root_node(), source.as_bytes());

        assert_eq!(includes.len(), 1);
        assert_eq!(includes[0].attributes.len(), 2);
        assert_eq!(
            includes[0].attributes[0],
            ("leveloffset".to_string(), "+1".to_string())
        );
        assert_eq!(
            includes[0].attributes[1],
            ("tags".to_string(), "main".to_string())
        );
    }

    #[test]
    fn test_extract_include_no_attributes() {
        let mut p = parser::init_block_parser().unwrap();
        let source = "= Title\n\ninclude::bare.adoc[]\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let includes = extract_includes(tree.root_node(), source.as_bytes());

        assert_eq!(includes.len(), 1);
        assert_eq!(includes[0].path, "bare.adoc");
        assert!(includes[0].attributes.is_empty());
    }

    #[test]
    fn test_extract_include_target_col() {
        let mut p = parser::init_block_parser().unwrap();
        let source = "= Title\n\ninclude::a.adoc[]\n\ninclude::{dir}/a.adoc[lines=1]\n";
        let tree = parser::parse(&mut p, source).unwrap();
        let includes = extract_includes(tree.root_node(), source.as_bytes());

        assert_eq!(includes.len(), 2);
        assert_eq!(includes[0].target_col, 9);
        assert_eq!(includes[1].target_col, 9);
        assert_eq!(includes[1].path, "{dir}/a.adoc");
    }

    // --- False-positive xref filtering ---

    #[test]
    fn test_xref_keyword_not_false_positive() {
        // The word "xref" in plain text must NOT produce a Xref
        let mut p = parser::init_inline_parser().unwrap();
        let source = "This text mentions xref without any macro syntax.";
        let tree = parser::parse(&mut p, source).unwrap();
        let xrefs = extract_xrefs(tree.root_node(), source.as_bytes());

        assert!(
            xrefs.is_empty(),
            "Plain text 'xref' must not produce false-positive Xrefs, got: {:?}",
            xrefs
        );
    }

    #[test]
    fn test_xref_shorthand_still_works() {
        // <<id,text>> form must still be detected
        let mut p = parser::init_inline_parser().unwrap();
        let source = "See <<my-section,Section Title>> for details.";
        let tree = parser::parse(&mut p, source).unwrap();
        let xrefs = extract_xrefs(tree.root_node(), source.as_bytes());

        assert_eq!(xrefs.len(), 1);
        assert_eq!(xrefs[0].target, "my-section");
        assert_eq!(xrefs[0].display_text.as_deref(), Some("Section Title"));
    }

    // --- extract_attribute_refs tests ---

    #[test]
    fn test_extract_attribute_refs_basic() {
        let source = "= Title\n\nThe {imagesdir} is here.\n";
        let refs = extract_attribute_refs(source);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "imagesdir");
        assert_eq!(refs[0].line, 3); // 1-based
        assert_eq!(refs[0].start_col, 4); // byte-offset of {
        assert_eq!(refs[0].end_col, 15); // exclusive end (after })
    }

    #[test]
    fn test_extract_attribute_refs_multiple_per_line() {
        let source = "Text {a} und {b} here.\n";
        let refs = extract_attribute_refs(source);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].name, "a");
        assert_eq!(refs[1].name, "b");
    }

    #[test]
    fn test_extract_attribute_refs_in_definition_line() {
        // :name: {other} → only {other} is a reference, :name: is a definition
        let source = ":name: {other}\n";
        let refs = extract_attribute_refs(source);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "other");
    }

    #[test]
    fn test_extract_attribute_refs_escaped() {
        // \{name} should be ignored
        let source = "Text \\{name} here.\n";
        let refs = extract_attribute_refs(source);
        assert!(
            refs.is_empty(),
            "Escaped attribute refs must be ignored, got: {:?}",
            refs
        );
    }

    #[test]
    fn test_extract_attribute_refs_empty_and_spaces() {
        // {} and {with spaces} should be ignored
        let source = "Text {} and {with spaces} here.\n";
        let refs = extract_attribute_refs(source);
        assert!(
            refs.is_empty(),
            "Empty/space-containing refs must be ignored, got: {:?}",
            refs
        );
    }

    // --- extract_conditionals tests ---

    #[test]
    fn test_extract_conditionals_ifdef() {
        let source = "= Title\n\nifdef::backend[]\nSome content.\nendif::backend[]\n";
        let result = extract_conditionals(source);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].kind, crate::types::ConditionalKind::Ifdef);
        assert_eq!(result[0].attributes[0], "backend");
        assert_eq!(result[0].line, 3);
    }

    #[test]
    fn test_extract_conditionals_ifndef() {
        let source = "= Title\n\nifndef::source-dir[]\nFallback.\nendif::source-dir[]\n";
        let result = extract_conditionals(source);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].kind, crate::types::ConditionalKind::Ifndef);
        assert_eq!(result[0].attributes[0], "source-dir");
        assert_eq!(result[0].line, 3);
    }

    #[test]
    fn test_extract_conditionals_mixed() {
        let source = "ifdef::backend[]\nContent.\nendif::[]\nifndef::icons[]\nMore.\nendif::[]\n";
        let result = extract_conditionals(source);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].kind, crate::types::ConditionalKind::Ifdef);
        assert_eq!(result[0].attributes[0], "backend");
        assert_eq!(result[1].kind, crate::types::ConditionalKind::Ifndef);
        assert_eq!(result[1].attributes[0], "icons");
    }

    #[test]
    fn test_extract_conditionals_and_operator() {
        let source = "ifdef::backend+icons[]\nContent.\nendif::[]\n";
        let result = extract_conditionals(source);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].attributes, vec!["backend", "icons"]);
        assert_eq!(result[0].operator, crate::types::ConditionalOperator::And);
        assert_eq!(result[0].line, 1);
    }

    #[test]
    fn test_extract_conditionals_or_operator() {
        let source = "ifndef::backend,icons[]\nFallback.\nendif::[]\n";
        let result = extract_conditionals(source);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].attributes, vec!["backend", "icons"]);
        assert_eq!(result[0].operator, crate::types::ConditionalOperator::Or);
        assert_eq!(result[0].kind, crate::types::ConditionalKind::Ifndef);
    }

    #[test]
    fn test_extract_conditionals_and_three_attrs() {
        let source = "ifdef::a+b+c[]\nendif::[]\n";
        let result = extract_conditionals(source);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].attributes, vec!["a", "b", "c"]);
        assert_eq!(result[0].operator, crate::types::ConditionalOperator::And);
    }

    #[test]
    fn test_extract_conditionals_invalid_multi_attr() {
        // Empty part after split should be rejected
        let source = "ifdef::backend+[]\n";
        let result = extract_conditionals(source);
        assert!(
            result.is_empty(),
            "Invalid multi-attr must be rejected, got: {result:?}"
        );
    }

    #[test]
    fn test_extract_conditionals_not_at_line_start() {
        let source = "Some text ifdef::attr[]\n";
        let result = extract_conditionals(source);
        assert!(
            result.is_empty(),
            "ifdef not at line start must be ignored, got: {result:?}"
        );
    }

    #[test]
    fn test_extract_conditionals_ignores_endif() {
        let source = "endif::attr[]\n";
        let result = extract_conditionals(source);
        assert!(result.is_empty(), "endif must be ignored, got: {result:?}");
    }

    #[test]
    fn test_xref_pagebreak_not_detected() {
        // <<< is a page break, not a cross-reference
        let mut p = parser::init_inline_parser().unwrap();
        let source = "<<<";
        let tree = parser::parse(&mut p, source).unwrap();
        let xrefs = extract_xrefs(tree.root_node(), source.as_bytes());

        assert!(
            xrefs.is_empty(),
            "Page break <<< must not be detected as xref, got: {:?}",
            xrefs
        );
    }

    #[test]
    fn test_xref_incomplete_not_detected() {
        // <<id without closing >> must not produce a xref
        let mut p = parser::init_inline_parser().unwrap();
        let source = "<<some-id";
        let tree = parser::parse(&mut p, source).unwrap();
        let xrefs = extract_xrefs(tree.root_node(), source.as_bytes());

        assert!(
            xrefs.is_empty(),
            "Incomplete xref <<id without >> must not be detected, got: {:?}",
            xrefs
        );
    }

    #[test]
    fn test_xref_in_backtick_not_detected() {
        // xref inside backtick code span must NOT produce a Xref
        let mut p = parser::init_inline_parser().unwrap();
        let source = "Use `xref:doc.adoc#anchor[link text]` for references.";
        let tree = parser::parse(&mut p, source).unwrap();
        let xrefs = extract_xrefs(tree.root_node(), source.as_bytes());

        assert!(
            xrefs.is_empty(),
            "Xref inside backticks must not be detected, got: {:?}",
            xrefs
        );
    }

    // --- Committed fixture: tests/fixtures/attribute_entries ---
    //
    // A `:to` line without closing colon and an attribute entry in a
    // section body. Each one used to turn the rest of the file into a parse
    // error, which hid its sections, attributes and inline references.

    fn attribute_entries_fixture() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/attribute_entries/main.adoc");
        std::fs::read_to_string(path).expect("attribute_entries fixture should exist")
    }

    #[test]
    fn test_fixture_attribute_entries_keep_the_sections_after_them() {
        let source = attribute_entries_fixture();
        let mut p = parser::init_block_parser().unwrap();
        let tree = parser::parse(&mut p, &source).unwrap();

        let sections: Vec<_> = extract_sections(tree.root_node(), source.as_bytes())
            .into_iter()
            .map(|s| (s.level, s.title))
            .collect();

        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        let expected = [
            (0, "Attribute entries in section bodies"),
            (1, "Completion probes"),
            (2, "Bare colon line"),
            (2, "After the bare line"),
            (1, "Body attributes"),
            (2, "Target"),
        ];
        let expected: Vec<_> = expected.iter().map(|(l, t)| (*l, t.to_string())).collect();
        assert_eq!(sections, expected);
    }

    #[test]
    fn test_fixture_colon_line_without_closing_colon_is_no_attribute() {
        let source = attribute_entries_fixture();
        let mut p = parser::init_block_parser().unwrap();
        let tree = parser::parse(&mut p, &source).unwrap();

        let attrs: Vec<_> = extract_attributes(tree.root_node(), source.as_bytes())
            .into_iter()
            .map(|a| (a.name, a.value, a.line))
            .collect();

        assert_eq!(attrs, [("product".to_string(), "Widget".to_string(), 20)]);
    }

    #[test]
    fn test_fixture_references_after_attribute_entries_are_found() {
        let source = attribute_entries_fixture();
        let mut p = parser::init_block_parser().unwrap();
        let tree = parser::parse(&mut p, &source).unwrap();

        let ranges = collect_inline_ranges(tree.root_node(), source.as_bytes());
        let (xrefs, _) = extract_inline_content_safe(&ranges, &source);
        let rows: Vec<_> = ranges.iter().map(|r| r.start_point.row).collect();
        let includes: Vec<_> = extract_includes(tree.root_node(), source.as_bytes())
            .into_iter()
            .map(|i| (i.path, i.line))
            .collect();

        assert_eq!(xrefs.len(), 1, "got {xrefs:?}");
        assert_eq!(xrefs[0].target, "_target");
        // `Uses {product} here.` on line 24.
        assert!(rows.contains(&23), "inline rows: {rows:?}");
        assert_eq!(includes, [("part.adoc".to_string(), 26)]);
    }

    /// `header.adoc`: a `:to` line in the header, typed on the way to
    /// `:toc:`, is a one-word author line (as in Asciidoctor) and must not
    /// carry on over the following lines into the first section.
    #[test]
    fn test_fixture_colon_line_in_header_keeps_the_sections() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/attribute_entries/header.adoc");
        let source = std::fs::read_to_string(path).expect("header fixture should exist");
        let mut p = parser::init_block_parser().unwrap();
        let tree = parser::parse(&mut p, &source).unwrap();

        let sections: Vec<_> = extract_sections(tree.root_node(), source.as_bytes())
            .into_iter()
            .map(|s| (s.level, s.title))
            .collect();
        let attrs: Vec<_> = extract_attributes(tree.root_node(), source.as_bytes())
            .into_iter()
            .map(|a| a.name)
            .collect();

        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        let expected = [
            (0, "Header probe"),
            (1, "Before the list"),
            (2, "After the list"),
        ];
        let expected: Vec<_> = expected.iter().map(|(l, t)| (*l, t.to_string())).collect();
        assert_eq!(sections, expected);
        assert_eq!(attrs, ["toc", "a"]);
    }

    /// Asciidoctor reads raw lines of an include target: any prefix before
    /// `tag::name[]` is fine, a backslash does not escape.
    #[rstest::rstest]
    #[case::hash("# tag::a[]")]
    #[case::slashes("//tag::a[]")]
    #[case::html_comment("<!-- tag::a[] -->")]
    #[case::bare("tag::a[]")]
    #[case::dash("-tag::a[]")]
    #[case::backslash("# \\tag::a[]")]
    fn test_extract_tag_directives_any_prefix(#[case] line: &str) {
        assert_eq!(extract_tag_directives(line), ["a"]);
    }

    #[test]
    fn test_extract_tag_directives_ignores_end_only() {
        assert_eq!(extract_tag_directives("// end::a[]"), Vec::<String>::new());
    }

    #[rstest::rstest]
    #[case::letters("# mytag::a[]")]
    #[case::underscore("_tag::a[]")]
    #[case::unicode_letter("\u{e9}tag::a[]")]
    fn test_extract_tag_directives_needs_word_boundary(#[case] line: &str) {
        assert_eq!(extract_tag_directives(line), Vec::<String>::new());
    }

    #[rstest::rstest]
    #[case::glued("tag::a[]x", &[])]
    #[case::tab("tag::a[]\tx", &[])]
    #[case::space("tag::a[] more", &["a"])]
    #[case::carriage_return("tag::a[]\r", &["a"])]
    fn test_extract_tag_directives_needs_space_or_eol_after_brackets(
        #[case] line: &str,
        #[case] expected: &[&str],
    ) {
        assert_eq!(extract_tag_directives(line), expected);
    }

    #[rstest::rstest]
    #[case::second_tag_ignored("// tag::a[] tag::b[]", &["a"])]
    #[case::end_first("# end::z[] tag::b[]", &[])]
    #[case::next_occurrence_after_glued("# mytag::x[] tag::b[]", &["b"])]
    fn test_extract_tag_directives_first_directive_per_line(
        #[case] line: &str,
        #[case] expected: &[&str],
    ) {
        assert_eq!(extract_tag_directives(line), expected);
    }

    #[test]
    fn test_extract_tag_directives_name_without_whitespace() {
        assert_eq!(
            extract_tag_directives("// tag::sp ace[]"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_extract_tag_directives_ignores_markup_and_conditionals() {
        let source =
            "////\n// tag::a[]\n////\n----\ntag::b[]\n----\nifdef::nope[]\n# tag::c[]\nendif::[]\n";
        assert_eq!(extract_tag_directives(source), ["a", "b", "c"]);
    }

    #[test]
    fn test_extract_tag_directives_is_case_sensitive() {
        assert_eq!(extract_tag_directives("# tag::Case[]"), ["Case"]);
    }

    /// `(context, delimiter, line)` of every unterminated block in `source`.
    fn unterminated(source: &str) -> Vec<(&'static str, String, usize)> {
        let mut p = parser::init_block_parser().unwrap();
        let tree = parser::parse(&mut p, source).unwrap();
        extract_unterminated_blocks(tree.root_node(), source.as_bytes())
            .into_iter()
            .map(|b| (b.context, b.delimiter, b.line))
            .collect()
    }

    #[test]
    fn test_unterminated_listing_block_reports_opener_line() {
        let source = "= Doc\n\nText.\n\n== Chapter\n\ntext\n\n----\ncode\n\n[[later]]\n== Later\n";
        assert_eq!(unterminated(source), [("listing", "----".to_string(), 9)]);
    }

    #[rstest::rstest]
    #[case::example("====", "example")]
    #[case::sidebar("****", "sidebar")]
    #[case::literal("....", "literal")]
    #[case::quote("____", "quote")]
    #[case::open("--", "open")]
    #[case::comment("////", "comment")]
    #[case::fenced("```", "listing")]
    #[case::long_listing("-----", "listing")]
    #[case::long_example("=====", "example")]
    fn test_unterminated_block_kinds(#[case] delimiter: &str, #[case] context: &'static str) {
        let source = format!("para\n\n{delimiter}\ncontent\n");
        assert_eq!(
            unterminated(&source),
            [(context, delimiter.to_string(), 3)],
            "{source:?}"
        );
    }

    /// The opener token includes trailing blanks; the message names the
    /// delimiter the user has to type.
    #[test]
    fn test_unterminated_block_delimiter_is_trimmed() {
        let source = "para\n\n----  \ncode\n";
        assert_eq!(unterminated(source), [("listing", "----".to_string(), 3)]);
    }

    #[test]
    fn test_unterminated_block_after_attribute_line_reports_delimiter_line() {
        assert_eq!(
            unterminated("para\n\n[source,rust]\n----\ncode\n"),
            [("listing", "----".to_string(), 4)]
        );
        assert_eq!(
            unterminated("para\n\n.Title\n----\ncode\n"),
            [("listing", "----".to_string(), 4)]
        );
    }

    #[test]
    fn test_unterminated_outer_block_with_closed_inner_block() {
        assert_eq!(
            unterminated("para\n\n====\n----\nx\n----\nrest\n"),
            [("example", "====".to_string(), 3)]
        );
    }

    #[test]
    fn test_closing_delimiter_of_other_length_leaves_block_unterminated() {
        let source = "p\n\n----\na\n----\n\n------\nb\n------\n\n----\nc\n------\n";
        assert_eq!(unterminated(source), [("listing", "----".to_string(), 11)]);
    }

    #[test]
    fn test_unterminated_listing_in_list_continuation() {
        assert_eq!(
            unterminated("* item\n+\n----\ncode\n"),
            [("listing", "----".to_string(), 3)]
        );
    }

    #[test]
    fn test_closed_blocks_report_nothing() {
        let sources = [
            "p\n\n----\na\n----\n",
            "p\n\n....\na\n....\n",
            "p\n\n====\na\n====\n",
            "p\n\n****\na\n****\n",
            "p\n\n____\na\n____\n",
            "p\n\n--\na\n--\n",
            "p\n\n////\na\n////\n",
            "p\n\n```\na\n```\n",
            "p\n\n----\n====\n----\n",
            "p\n\n////\n----\n////\n",
            "p\n\n|===\na|\n----\nx\n----\n\n|===\n",
            "* item\n+\n----\ncode\n----\n",
            "p\n\n***\n",
        ];
        for source in sources {
            assert_eq!(unterminated(source), [], "{source:?}");
        }
    }

    /// Asciidoctor accepts trailing blanks on a delimiter line; the grammar
    /// does too, so a correct block closed by `----  ` reports nothing.
    #[test]
    fn test_closing_delimiter_with_trailing_blanks_reports_nothing() {
        for source in [
            "p\n\n----\na\n----  \n",
            "p\n\n----  \na\n----\n",
            "p\n\n====\na\n==== \t\n",
        ] {
            assert_eq!(unterminated(source), [], "{source:?}");
        }
    }

    /// A closing delimiter on the last line without a newline closes the block.
    #[test]
    fn test_closing_delimiter_at_end_of_file_without_newline_reports_nothing() {
        for source in ["p\n\n----\na\n----", "p\n\n====\na\n===="] {
            assert_eq!(unterminated(source), [], "{source:?}");
        }
    }

    /// Asciidoctor closes a fenced block only with the bare fence, so the
    /// message names the fence without the language.
    #[test]
    fn test_unterminated_fenced_block_with_language_names_bare_fence() {
        assert_eq!(
            unterminated("p\n\n```ruby\nputs 1\n"),
            [("listing", "```".to_string(), 3)]
        );
    }

    /// An open comment inside a container ends with a zero-width marker and
    /// swallows the outer closer; Asciidoctor warns only for the comment.
    #[test]
    fn test_open_comment_inside_container_reports_only_comment() {
        let sources = [
            "p\n\n====\n////\nx\n====\n",
            "p\n\n****\n////\nx\n****\n",
            "p\n\n--\n////\nx\n--\n",
            "* item\n+\n====\n////\nx\n====\n",
        ];
        for source in sources {
            let line = source.lines().position(|l| l == "////").unwrap() + 1;
            assert_eq!(
                unterminated(source),
                [("comment", "////".to_string(), line)],
                "{source:?}"
            );
        }
    }

    /// The opener on the last line without a newline is still reported.
    #[test]
    fn test_unterminated_opener_at_end_of_file_without_newline() {
        assert_eq!(
            unterminated("p\n\n----"),
            [("listing", "----".to_string(), 3)]
        );
    }

    /// Asciidoctor warns for these; the grammar yields an `ERROR` tree
    /// without pairing, so no diagnostic is better than a guessed one.
    #[test]
    fn test_unterminated_block_in_grammar_error_tree_reports_nothing() {
        let sources = [
            "p\n\n++++\nraw\n",
            "p\n\n|===\na|b\n",
            "p\n\n====\n----\ncode\n====\n",
        ];
        for source in sources {
            assert_eq!(unterminated(source), [], "{source:?}");
        }
    }
}
