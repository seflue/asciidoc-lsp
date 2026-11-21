//! What the conditional directives of a file say, line by line, without
//! evaluating them: the directives themselves, and the lines their blocks
//! cover.

use super::{directive, BlockStack};
use crate::types::{ConditionalDirective, Directive};

/// The `ifdef` and `ifndef` directives of `source`, in order, with their
/// 1-based lines. `endif`, `ifeval` and the lines the reader drops are not
/// among them.
#[must_use]
pub fn extract_conditionals(source: &str) -> Vec<ConditionalDirective> {
    source
        .lines()
        .enumerate()
        .filter_map(|(index, line)| match directive(line)? {
            Directive::Condition {
                kind,
                names,
                operator,
                ..
            } => Some(ConditionalDirective {
                kind,
                attributes: names,
                operator,
                line: index + 1,
            }),
            _ => None,
        })
        .collect()
}

/// One line of a file as the reader sees it, except in a `////` comment
/// block: the reader skips preprocessing there, a directive in it is no
/// directive, but this layer reads it as one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadLine {
    /// The directive the line is, `None` for a text line.
    pub directive: Option<Directive>,
    /// Whether a block conditional is open at the line: the line is in
    /// the content of an `ifdef`, `ifndef` or `ifeval` that has no `endif`
    /// yet. A directive line inside a block counts too.
    pub in_block: bool,
}

impl ReadLine {
    /// Whether the line is a conditional directive (`ifdef`, `ifndef`,
    /// `ifeval`, `endif`, or one the reader drops), that is a directive
    /// but no `include`.
    #[must_use]
    pub fn is_conditional(&self) -> bool {
        matches!(
            self.directive,
            Some(
                Directive::Condition { .. }
                    | Directive::Ifeval
                    | Directive::Endif { .. }
                    | Directive::Dropped
            )
        )
    }
}

/// The lines of `source` in the state of the reader's conditional stack.
///
/// The block form of a conditional opens a block, the single-line form
/// (`ifdef::a[text]`) opens none. `endif` closes the innermost block, but
/// only if it carries no target or the target of that block: the reader
/// ignores a stray or mismatching `endif` with an error. A line it drops
/// opens and closes nothing. Conditionals are not evaluated.
#[must_use]
pub fn read_lines(source: &str) -> Vec<ReadLine> {
    let mut open = BlockStack::new();
    source
        .lines()
        .map(|line| {
            let directive = directive(line);
            let in_block = !open.is_empty();
            match &directive {
                Some(Directive::Condition {
                    names,
                    operator,
                    content: None,
                    ..
                }) => open.open_condition(names, *operator, ()),
                Some(Directive::Ifeval) => open.open_ifeval(()),
                Some(Directive::Endif { target }) => {
                    open.close(target.as_deref());
                }
                _ => {}
            }
            ReadLine {
                directive,
                in_block,
            }
        })
        .collect()
}

/// Per 0-based line of `source`: a conditional directive (`ifdef`, `ifndef`,
/// `ifeval`, `endif`, or one the reader drops) or a line inside a block
/// conditional. The LSP does not evaluate conditionals, so it cannot tell
/// whether such a line is rendered.
#[must_use]
pub fn conditional_lines(source: &str) -> Vec<bool> {
    read_lines(source)
        .iter()
        .map(|line| line.in_block || line.is_conditional())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ConditionalKind, ConditionalOperator};

    // The tests of `extract_conditionals`, moved from the extractors.

    #[test]
    fn test_extract_conditionals_ifdef() {
        let source = "= Title\n\nifdef::backend[]\nSome content.\nendif::backend[]\n";
        let result = extract_conditionals(source);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].kind, ConditionalKind::Ifdef);
        assert_eq!(result[0].attributes[0], "backend");
        assert_eq!(result[0].line, 3);
    }

    #[test]
    fn test_extract_conditionals_ifndef() {
        let source = "= Title\n\nifndef::source-dir[]\nFallback.\nendif::source-dir[]\n";
        let result = extract_conditionals(source);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].kind, ConditionalKind::Ifndef);
        assert_eq!(result[0].attributes[0], "source-dir");
        assert_eq!(result[0].line, 3);
    }

    #[test]
    fn test_extract_conditionals_mixed() {
        let source = "ifdef::backend[]\nContent.\nendif::[]\nifndef::icons[]\nMore.\nendif::[]\n";
        let result = extract_conditionals(source);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].kind, ConditionalKind::Ifdef);
        assert_eq!(result[0].attributes[0], "backend");
        assert_eq!(result[1].kind, ConditionalKind::Ifndef);
        assert_eq!(result[1].attributes[0], "icons");
    }

    #[test]
    fn test_extract_conditionals_and_operator() {
        let source = "ifdef::backend+icons[]\nContent.\nendif::[]\n";
        let result = extract_conditionals(source);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].attributes, vec!["backend", "icons"]);
        assert_eq!(result[0].operator, ConditionalOperator::And);
        assert_eq!(result[0].line, 1);
    }

    #[test]
    fn test_extract_conditionals_or_operator() {
        let source = "ifndef::backend,icons[]\nFallback.\nendif::[]\n";
        let result = extract_conditionals(source);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].attributes, vec!["backend", "icons"]);
        assert_eq!(result[0].operator, ConditionalOperator::Or);
        assert_eq!(result[0].kind, ConditionalKind::Ifndef);
    }

    #[test]
    fn test_extract_conditionals_and_three_attrs() {
        let source = "ifdef::a+b+c[]\nendif::[]\n";
        let result = extract_conditionals(source);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].attributes, vec!["a", "b", "c"]);
        assert_eq!(result[0].operator, ConditionalOperator::And);
    }

    /// The reader splits `a+` into `a` and an empty name, which is never
    /// defined (Asciidoctor 2.0.26: `ifdef::foo+[]` hides its block with
    /// `foo` set).
    #[test]
    fn test_extract_conditionals_keeps_empty_name_of_multi_attr() {
        let result = extract_conditionals("ifdef::backend+[]\n");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].attributes, vec!["backend", ""]);
        assert_eq!(result[0].operator, ConditionalOperator::And);
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

    /// Where the old scan of `extract_conditionals` and the reader differed:
    /// names are lowered and may hold any non-space character, a line that
    /// does not end in `]` or has whitespace in the target is text, an
    /// escaped or indented one too.
    #[test]
    fn test_extract_conditionals_follows_the_reader() {
        let names = |source: &str| -> Vec<Vec<String>> {
            extract_conditionals(source)
                .into_iter()
                .map(|c| c.attributes)
                .collect()
        };
        assert_eq!(names("ifdef::FOO[]\n"), [["foo"]]);
        assert_eq!(names("ifdef::a.b[]\n"), [["a.b"]]);
        assert_eq!(names("ifdef::a+b,c[]\n"), [["a", "b,c"]]);
        assert_eq!(names("ifdef::foo[] trailing\n"), Vec::<Vec<String>>::new());
        assert_eq!(names("ifdef::foo[]  \n"), [["foo"]]);
        assert_eq!(names(" ifdef::foo[]\n"), Vec::<Vec<String>>::new());
        assert_eq!(names("\\ifdef::foo[]\n"), Vec::<Vec<String>>::new());
        assert_eq!(names("ifdef::a b[]\n"), Vec::<Vec<String>>::new());
        assert_eq!(names("ifdef::[]\n"), Vec::<Vec<String>>::new());
    }

    #[test]
    fn a_block_covers_its_lines_until_its_endif() {
        assert_eq!(
            conditional_lines(
                "a\nifdef::g[]\nb\nifeval::[1 == 1]\nc\nendif::[]\nd\nendif::[]\ne\n"
            ),
            [false, true, true, true, true, true, true, true, false]
        );
    }

    #[test]
    fn a_single_line_conditional_opens_no_block() {
        assert_eq!(conditional_lines("ifdef::g[text]\nb\n"), [true, false]);
    }

    /// The reader drops a line of a directive it rejects with an error and
    /// opens or closes nothing (checked against
    /// Asciidoctor 2.0.26), or takes it for a text line.
    #[test]
    fn dropped_and_text_lines_open_and_close_nothing() {
        assert_eq!(conditional_lines("ifdef::a b[]\nX\n"), [false, false]);
        assert_eq!(conditional_lines("ifdef::[]\nX\n"), [true, false]);
        assert_eq!(
            conditional_lines("ifdef::g[]\nendif::x[text]\nY\nendif::[]\nZ\n"),
            [true, true, true, true, false]
        );
        assert_eq!(
            conditional_lines("ifeval::[nonsense]\nX\nendif::[]\nY\n"),
            [true, false, true, false]
        );
    }

    /// An `endif` with another target than the open block is ignored, one
    /// without a block too (checked against Asciidoctor 2.0.26: `mismatched
    /// preprocessor directive`).
    #[test]
    fn an_endif_for_another_target_closes_nothing() {
        assert_eq!(
            conditional_lines("ifdef::a[]\nendif::b[]\nX\nendif::A[]\nY\n"),
            [true, true, true, true, false]
        );
        assert_eq!(
            conditional_lines("ifdef::a,b[]\nendif::a[]\nX\nendif::a,b[]\nY\n"),
            [true, true, true, true, false]
        );
        assert_eq!(conditional_lines("endif::[]\nX\n"), [true, false]);
        assert_eq!(
            conditional_lines("ifeval::[1 == 1]\nendif::a[]\nX\nendif::[]\nY\n"),
            [true, true, true, true, false]
        );
    }
}
