//! The one recognizer for preprocessor directive lines, after Asciidoctor's
//! `ConditionalDirectiveRx` and `IncludeDirectiveRx` (`rx.rb`) and
//! `Reader#process_line` (`reader.rb`, 2.0.26).

use crate::types::{ConditionalKind, ConditionalOperator, Directive};

/// Whitespace as the `\s` and `\S` of the reader's expressions see it.
fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

/// The directive `line` is, `None` when the reader takes it for text.
///
/// The reader works on lines without trailing whitespace. A directive starts
/// the line (an indented one is text, so is a `\`-escaped one), ends in `]`
/// and does not start with `[`. Keywords are lower case, the attribute names
/// of a conditional are not (they are lowered). A conditional target holds no
/// whitespace; an include target may, inside.
#[must_use]
pub fn directive(line: &str) -> Option<Directive> {
    let line = line.trim_end_matches(is_space);
    if !line.ends_with(']') || line.starts_with('[') {
        return None;
    }
    match line.strip_prefix("include::") {
        Some(rest) => include(rest),
        None => conditional(line),
    }
}

fn include(rest: &str) -> Option<Directive> {
    let (target, _) = rest.split_once('[')?;
    let first = target.chars().next()?;
    let last = target.chars().next_back()?;
    (!is_space(first) && !is_space(last)).then(|| Directive::Include {
        target: target.to_string(),
    })
}

fn conditional(line: &str) -> Option<Directive> {
    let (name, rest) = ["ifdef", "ifndef", "ifeval", "endif"]
        .into_iter()
        .find_map(|name| Some((name, line.strip_prefix(name)?.strip_prefix("::")?)))?;
    let (target, text) = rest.split_once('[')?;
    if target.contains(is_space) {
        return None;
    }
    let text = text.strip_suffix(']').filter(|text| !text.is_empty());
    Some(match name {
        "endif" if text.is_some() => Directive::Dropped,
        "endif" => Directive::Endif {
            target: (!target.is_empty()).then(|| target.to_lowercase()),
        },
        "ifeval" => match text {
            Some(text) if target.is_empty() && is_expression(text.trim_matches(is_space)) => {
                Directive::Ifeval
            }
            _ => Directive::Dropped,
        },
        _ if target.is_empty() => Directive::Dropped,
        _ => {
            let (operator, delimiter) = match target.find([',', '+']) {
                Some(at) if target.as_bytes()[at] == b',' => (ConditionalOperator::Or, ','),
                Some(_) => (ConditionalOperator::And, '+'),
                None => (ConditionalOperator::Single, ','),
            };
            Directive::Condition {
                kind: if name == "ifdef" {
                    ConditionalKind::Ifdef
                } else {
                    ConditionalKind::Ifndef
                },
                names: target
                    .to_lowercase()
                    .split(delimiter)
                    .map(str::to_string)
                    .collect(),
                operator,
                content: text.map(str::to_string),
            }
        }
    })
}

/// Whether `text` matches `EvalExpressionRx`: something, a comparison
/// operator (`==`, `!=`, `<=`, `>=`, `<`, `>`) between optional spaces, and
/// something.
fn is_expression(text: &str) -> bool {
    text.char_indices().skip(1).any(|(at, _)| {
        let rest = text[at..].trim_start_matches(' ');
        let two = ["==", "!=", "<=", ">="]
            .iter()
            .any(|op| rest.strip_prefix(op).is_some_and(|rhs| !rhs.is_empty()));
        let one = rest
            .strip_prefix(['<', '>'])
            .is_some_and(|rhs| !rhs.is_empty());
        two || one
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn condition(
        kind: ConditionalKind,
        names: &[&str],
        operator: ConditionalOperator,
        content: Option<&str>,
    ) -> Option<Directive> {
        Some(Directive::Condition {
            kind,
            names: names.iter().map(ToString::to_string).collect(),
            operator,
            content: content.map(ToString::to_string),
        })
    }

    fn ifdef(names: &[&str]) -> Option<Directive> {
        condition(
            ConditionalKind::Ifdef,
            names,
            ConditionalOperator::Single,
            None,
        )
    }

    fn endif(target: Option<&str>) -> Option<Directive> {
        Some(Directive::Endif {
            target: target.map(ToString::to_string),
        })
    }

    fn include_of(target: &str) -> Option<Directive> {
        Some(Directive::Include {
            target: target.to_string(),
        })
    }

    /// One line per row: the case of the Asciidoctor 2.0.26 runs the line
    /// belongs to, the line, and what the reader made of it.
    #[test]
    fn conditionals_as_asciidoctor_reads_them() {
        use ConditionalKind::{Ifdef, Ifndef};
        use ConditionalOperator::{And, Or, Single};
        let dropped = || Some(Directive::Dropped);
        let cases: Vec<(&str, &str, Option<Directive>)> = vec![
            ("c01", "ifdef::foo[]", ifdef(&["foo"])),
            // The name is lowered: `FOO` is `-a foo`.
            ("c02", "ifdef::FOO[]", ifdef(&["foo"])),
            // Text after the `]` makes it a text line.
            ("c03", "ifdef::nope[] trailing", None),
            // The reader strips trailing whitespace.
            ("c04", "ifdef::nope[]   ", ifdef(&["nope"])),
            // Indented and escaped lines are text.
            ("c05", " ifdef::nope[]", None),
            ("c05", " endif::[]", None),
            ("c06", "\\ifdef::nope[]", None),
            ("c06", "\\endif::[]", None),
            (
                "c07",
                "ifdef::nope[SINGLE]",
                condition(Ifdef, &["nope"], Single, Some("SINGLE")),
            ),
            // No target: the reader drops the line with an error.
            ("c09", "ifdef::[]", dropped()),
            ("c09", "ifndef::[]", dropped()),
            ("c09", "ifdef::[text]", dropped()),
            // Text in an `endif`: dropped, closes nothing.
            ("c10", "endif::[x]", dropped()),
            ("c11", "endif::other[]", endif(Some("other"))),
            ("c27", "endif::FOO[]", endif(Some("foo"))),
            ("c01", "endif::[]", endif(None)),
            // Whitespace in the target: text.
            ("c12", "ifdef::a b[]", None),
            ("c28", "ifdef:: foo[]", None),
            ("c29", "ifdef::foo\t[]", None),
            // No restriction on the characters of a name.
            ("c13", "ifdef::a.b[]", ifdef(&["a.b"])),
            ("c14", "ifeval::[1 == 1]", Some(Directive::Ifeval)),
            ("c15", "ifeval::[1 == 2]", Some(Directive::Ifeval)),
            ("e04", "ifeval::[1 <= 2]", Some(Directive::Ifeval)),
            ("e05", "ifeval::[\"a\"==\"b\"]", Some(Directive::Ifeval)),
            // `ifeval` takes no target and needs an expression: dropped.
            ("c16", "ifeval::x[1 == 1]", dropped()),
            ("c17", "ifeval::[nonsense]", dropped()),
            ("e01", "ifeval::[1 = 1]", dropped()),
            ("e02", "ifeval::[1 ==]", dropped()),
            ("e03", "ifeval::[ ]", dropped()),
            ("e03", "ifeval::[]", dropped()),
            ("e06", "ifeval::[1 > ]", dropped()),
            ("e07", "ifeval::[== 1]", dropped()),
            // Keywords are lower case; a leading `[` makes it text.
            ("c18", "IFDEF::foo[]", None),
            ("c22", "[ifdef::nope[]]", None),
            (
                "c19",
                "ifdef::foo,nope[]",
                condition(Ifdef, &["foo", "nope"], Or, None),
            ),
            (
                "c20",
                "ifdef::foo+nope[]",
                condition(Ifdef, &["foo", "nope"], And, None),
            ),
            (
                "c24",
                "ifndef::foo[]",
                condition(Ifndef, &["foo"], Single, None),
            ),
            // The first of `,` and `+` is the delimiter.
            (
                "c21",
                "ifdef::nope,foo+x[]",
                condition(Ifdef, &["nope", "foo+x"], Or, None),
            ),
            (
                "c21",
                "ifdef::a+b,c[]",
                condition(Ifdef, &["a", "b,c"], And, None),
            ),
            // Empty names stay, as in `split(',', -1)`.
            (
                "c30",
                "ifdef::nope,,foo[]",
                condition(Ifdef, &["nope", "", "foo"], Or, None),
            ),
            // The text runs to the last `]`.
            (
                "c25",
                "ifdef::foo[a] b]",
                condition(Ifdef, &["foo"], Single, Some("a] b")),
            ),
        ];
        for (case, line, expected) in cases {
            assert_eq!(directive(line), expected, "{case}: {line:?}");
        }
    }

    #[test]
    fn includes_as_asciidoctor_reads_them() {
        let cases: Vec<(&str, &str, Option<Directive>)> = vec![
            ("i01", "include::inc.adoc[]", include_of("inc.adoc")),
            // Whitespace inside the target is part of it.
            ("i02", "include::my inc.adoc[]", include_of("my inc.adoc")),
            // At its start or end it makes the line text.
            ("i03", "include:: inc.adoc[]", None),
            ("i04", "include::inc.adoc []", None),
            ("i05", "include::[]", None),
            ("i06", "\\include::inc.adoc[]", None),
            ("i07", "include::inc.adoc[", None),
            ("i08", "include::inc.adoc[] x", None),
            ("i09", " include::inc.adoc[]", None),
            ("i10", "include::inc.adoc[]   ", include_of("inc.adoc")),
            ("i11", "INCLUDE::inc.adoc[]", None),
            ("i12", "include::inc.adoc[lines=1]", include_of("inc.adoc")),
            ("i13", "include::{inc}[]", include_of("{inc}")),
            ("i17", "include::inc.adoc[a]b[]", include_of("inc.adoc")),
            ("i16", "include::nofile.adoc[]", include_of("nofile.adoc")),
        ];
        for (case, line, expected) in cases {
            assert_eq!(directive(line), expected, "{case}: {line:?}");
        }
    }

    /// A single-line conditional around an include is a conditional; its
    /// content is read again by the reader (case i14).
    #[test]
    fn include_in_single_line_conditional_is_its_content() {
        assert_eq!(
            directive("ifdef::foo[include::inc.adoc[]]"),
            condition(
                ConditionalKind::Ifdef,
                &["foo"],
                ConditionalOperator::Single,
                Some("include::inc.adoc[]")
            )
        );
    }

    #[test]
    fn plain_text_is_no_directive() {
        for line in ["", "text", "ifdef", "ifdef::", "tag::a[]", "image::a.png[]"] {
            assert_eq!(directive(line), None, "{line:?}");
        }
    }
}
