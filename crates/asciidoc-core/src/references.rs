use std::collections::HashMap;
use std::path::PathBuf;

use crate::types::DocumentModel;

/// The document a file reference names: `resolved` when it is one, else the
/// first one whose path ends in `file_ref` or is named `file_ref`.
#[must_use]
pub fn find_target_file<'a>(
    documents: &'a HashMap<PathBuf, DocumentModel>,
    resolved: Option<&PathBuf>,
    file_ref: &str,
) -> Option<&'a PathBuf> {
    resolved
        .and_then(|p| documents.keys().find(|key| *key == p))
        .or_else(|| {
            documents.keys().find(|p| {
                p.ends_with(file_ref)
                    || p.file_name()
                        .is_some_and(|f| f.to_string_lossy() == file_ref)
            })
        })
}

/// Whether the xref `target` names a file (`file.adoc#id`, `file.adoc`), as
/// opposed to an ID in the current document.
#[must_use]
pub fn xref_names_file(target: &str) -> bool {
    parse_xref_target(target).0.is_some()
}

/// Parse an xref target into optional file part and optional anchor part.
/// Examples:
///   "file.adoc#anchor" → (Some("file.adoc"), Some("anchor"))
///   "#anchor"          → (None, Some("anchor"))
///   "file.adoc"        → (Some("file.adoc"), None)
///   "anchor"           → (None, Some("anchor"))
#[must_use]
pub fn parse_xref_target(target: &str) -> (Option<&str>, Option<&str>) {
    if let Some((file, anchor)) = target.split_once('#') {
        let file_part = if file.is_empty() { None } else { Some(file) };
        let anchor_part = if anchor.is_empty() {
            None
        } else {
            Some(anchor)
        };
        (file_part, anchor_part)
    } else if target.contains('.') {
        // Looks like a filename (has extension)
        (Some(target), None)
    } else {
        // Plain anchor ID
        (None, Some(target))
    }
}

/// Generate an implicit section ID from a title.
/// Rules: lowercase, non-alnum → separator, prepend prefix.
#[must_use]
pub fn generate_section_id(title: &str, prefix: &str, separator: &str) -> String {
    let mut result = String::with_capacity(prefix.len() + title.len());
    result.push_str(prefix);

    let mut prev_was_sep = !prefix.is_empty(); // avoid double separator after prefix
    for ch in title.chars() {
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                result.push(lower);
            }
            prev_was_sep = false;
        } else if !prev_was_sep {
            result.push_str(separator);
            prev_was_sep = true;
        }
    }

    // Remove trailing separator
    while result.ends_with(separator) && result.len() > prefix.len() {
        result.truncate(result.len() - separator.len());
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    // --- Section ID generation ---

    #[rstest]
    #[case("Building Block View", "_", "_", "_building_block_view")]
    #[case("What is \"Clean Code\"?", "_", "_", "_what_is_clean_code")]
    #[case("Building Block View", "sec-", "-", "sec-building-block-view")]
    #[case("Building Block View", "", "_", "building_block_view")]
    fn test_generate_section_id(
        #[case] title: &str,
        #[case] prefix: &str,
        #[case] separator: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(generate_section_id(title, prefix, separator), expected);
    }

    // --- Committed fixtures: tests/fixtures/{explicit_section_ids,book_parts,leveloffset_*,nested_include,book_part_include} ---
    //
    // Asciidoctor generates no `_title` ID for a section that carries an
    // explicit ID, nor for the document header title. The xref completion
    // reads the same catalogs, so the assertions on them cover it too.

    fn fixture_workspace(name: &str) -> crate::workspace::Workspace {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(name);
        crate::workspace::Workspace::build(&root, &root.join("index.adoc"))
    }

    /// Whether some top-level document of the workspace defines `id`.
    fn defines(ws: &crate::workspace::Workspace, id: &str) -> bool {
        ws.anchor_definitions()
            .iter()
            .any(|(defined, _)| defined == id)
    }

    fn broken_targets(ws: &crate::workspace::Workspace) -> Vec<String> {
        let mut broken: Vec<String> = ws
            .all_diagnostics()
            .into_iter()
            .filter_map(|d| match d.kind {
                crate::types::DiagnosticKind::BrokenXref { target, .. } => Some(target),
                _ => None,
            })
            .collect();
        broken.sort();
        broken
    }

    #[test]
    fn test_fixture_explicit_section_ids_suppress_implicit_ids() {
        let ws = fixture_workspace("explicit_section_ids");

        assert_eq!(
            broken_targets(&ws),
            [
                "_audience",
                "_by_attr",
                "_inline_anchor_tid",
                "_introduction",
                "_product_manual",
                "_stacked",
                "nsp"
            ]
        );
        // Not an anchor in Asciidoctor (escaped, invalid ID start, no space
        // before `[[`): the section keeps its implicit ID.
        for id in ["_escaped_esc", "_bad_anchor_1bad", "_nospace"] {
            assert!(defines(&ws, id), "{id}");
        }
        // Attribute lists without an ID keep the implicit ID.
        for id in ["_options", "_with_role", "_appx"] {
            assert!(defines(&ws, id), "{id}");
        }
        assert!(!defines(&ws, "_introduction"));
        assert!(defines(&ws, "tid"));
        // `[[id,]]` (empty reftext) is no section anchor: the anchor text
        // becomes part of the title.
        assert!(defines(&ws, "_empty_reftext_etid"));
        assert!(!defines(&ws, "etid"));
    }

    #[test]
    fn test_fixture_included_title_with_block_id_has_no_implicit_id() {
        let ws = fixture_workspace("included_title_block_id");

        assert_eq!(broken_targets(&ws), ["_ch_one", "_ch_three", "_ch_two"]);
        // `[#id]` / `[id=]` are not indexed as anchors yet (separate backlog
        // item), so only `[[cid1]]` is asserted as an anchor.
        for id in ["cid1", "_ch_four_with_role"] {
            assert!(defines(&ws, id), "{id}");
        }
        assert!(!defines(&ws, "_ch_one"));
    }

    #[test]
    fn test_fixture_headerless_book_part_keeps_implicit_id() {
        let ws = fixture_workspace("headerless_book");

        assert!(broken_targets(&ws).is_empty());
        assert!(defines(&ws, "_part_one"));
    }

    #[test]
    fn test_fixture_book_part_keeps_implicit_id_header_title_does_not() {
        let ws = fixture_workspace("book_parts");

        assert_eq!(broken_targets(&ws), ["_my_book"]);
        assert!(defines(&ws, "_part_one"));
    }

    #[test]
    fn test_fixture_leveloffset_argument_include_keeps_title_id() {
        let ws = fixture_workspace("leveloffset_chapter");

        assert_eq!(broken_targets(&ws), ["_main_doc"]);
        assert!(defines(&ws, "_chapter_foo"));
    }

    #[test]
    fn test_fixture_leveloffset_attribute_include_keeps_title_id() {
        let ws = fixture_workspace("leveloffset_attribute");

        assert!(broken_targets(&ws).is_empty());
        assert!(defines(&ws, "_chapter_foo"));
    }

    #[test]
    fn test_fixture_nested_include_keeps_title_id() {
        let ws = fixture_workspace("nested_include");

        assert!(broken_targets(&ws).is_empty());
        assert!(defines(&ws, "_inner_title"));
    }

    #[test]
    fn test_fixture_book_part_include_keeps_title_id() {
        let ws = fixture_workspace("book_part_include");

        assert!(broken_targets(&ws).is_empty());
        assert!(defines(&ws, "_chapter_foo"));
    }
}
