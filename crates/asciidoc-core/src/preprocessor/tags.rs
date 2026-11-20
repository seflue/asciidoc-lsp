//! The tag directives of a file: `tag::name[]` and `end::name[]` in the
//! raw lines of an include target.

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
