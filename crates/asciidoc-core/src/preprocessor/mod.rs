//! The preprocessor layer: what Asciidoctor's reader takes from the raw
//! lines before any parser sees them. Directives (`ifdef`, `ifndef`,
//! `ifeval`, `endif`, `include`) and the tag directives of an include target
//! are recognized here and only here; the rest of the crate gets
//! [`Directive`]s, [`ReadLine`]s and tag names, never a line to test again.
//!
//! [`directive`] follows Asciidoctor 2.0.26 (`rx.rb`, `reader.rb`). Where an
//! earlier recognizer judged a line differently, the reader decides: see the
//! cases in `directive.rs`, each one run through Asciidoctor.
//!
//! Limit: the reader does not preprocess the lines of a `////` comment
//! block, a directive in it is text. The layer does not know the blocks and
//! reads such a line as a directive.
//!
//! Not in this module: the header lines (`extract_header_lines`, which only
//! asks [`directive`]) and the reading order over includes (`reading`), which
//! work on what the layer recognized.

mod blocks;
mod conditionals;
mod directive;
mod tags;

pub use blocks::BlockStack;
pub use conditionals::{conditional_lines, extract_conditionals, read_lines, ReadLine};
pub use directive::directive;
pub use tags::extract_tag_directives;

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    /// Directive keywords as they stand in a line check.
    const KEYWORDS: [&str; 7] = [
        "ifdef", "ifndef", "ifeval", "endif", "include", "tag", "end",
    ];

    /// The keywords of the directives that start a line.
    const DIRECTIVES: [&str; 5] = ["ifdef", "ifndef", "ifeval", "endif", "include"];

    /// Calls that cut a line at its `::`.
    const SPLITS: [&str; 3] = ["split_once(\"::\")", "strip_prefix(\"::\")", "find(\"::\")"];

    /// String methods a line check is made of.
    const LINE_TESTS: [&str; 6] = [
        "starts_with",
        "strip_prefix",
        "ends_with",
        "contains",
        "find",
        "split_once",
    ];

    fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name != "preprocessor") {
                    sources(&path, found);
                }
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
    }

    /// The code lines of `text` that test a line for a directive:
    /// - a string literal `"<keyword>::` (the start of a directive or a tag
    ///   directive),
    /// - a keyword handed to a line test (`starts_with("ifdef")`),
    /// - two directive keywords side by side in a literal list
    ///   (`["ifdef", "ifndef"]`, to find the one a line starts with),
    /// - in a file that splits at `"::"`, a keyword in a `matches!` list
    ///   (`"ifdef" | "endif"`).
    ///
    /// Comment lines and the unit tests at the bottom of a file (a
    /// `#[cfg(test)]` directly before `mod tests`) do not count. Limits: a
    /// file that does not split at `"::"` may name the keywords in a
    /// `matches!` (the node names of the grammar do), and a literal that
    /// builds directive text (`format!("include::{target}[]")`) or a
    /// directive in a trailing code comment is flagged although nothing is
    /// tested. What does not start with the keyword in a literal, or builds
    /// it from parts, is not seen.
    fn line_checks(text: &str) -> Vec<(usize, &str)> {
        let mut needles = Vec::new();
        for keyword in KEYWORDS {
            needles.push(format!("\"{keyword}::"));
            needles.push(format!("\"\\\\{keyword}::"));
            for test in LINE_TESTS {
                needles.push(format!(".{test}(\"{keyword}\""));
            }
        }
        for first in DIRECTIVES {
            for second in DIRECTIVES.iter().filter(|second| **second != first) {
                needles.push(format!("\"{first}\", \"{second}\""));
            }
        }
        let lines: Vec<&str> = text.lines().collect();
        let end = (0..lines.len())
            .find(|&index| {
                lines[index].trim() == "#[cfg(test)]"
                    && lines
                        .get(index + 1)
                        .is_some_and(|next| next.trim_start().starts_with("mod tests"))
            })
            .unwrap_or(lines.len());
        let code: Vec<(usize, &str)> = lines[..end]
            .iter()
            .enumerate()
            .filter(|(_, line)| !line.trim_start().starts_with("//"))
            .map(|(index, line)| (index + 1, *line))
            .collect();
        if code
            .iter()
            .any(|(_, line)| SPLITS.iter().any(|split| line.contains(split)))
        {
            for keyword in DIRECTIVES {
                needles.push(format!("\"{keyword}\" |"));
                needles.push(format!("| \"{keyword}\""));
            }
        }
        code.into_iter()
            .filter(|(_, line)| needles.iter().any(|needle| line.contains(needle.as_str())))
            .collect()
    }

    /// Fails when code outside `preprocessor/` recognizes a directive by
    /// testing a line again. Ask [`super::directive`] instead.
    #[test]
    fn no_line_check_for_directives_outside_the_module() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        sources(&src, &mut files);
        let mut found = Vec::new();
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            for (line, code) in line_checks(&text) {
                found.push(format!("{}:{line}: {}", file.display(), code.trim()));
            }
        }
        assert!(
            found.is_empty(),
            "directives are recognized in preprocessor/ only; use \
             preprocessor::directive instead of a line check:\n{}",
            found.join("\n")
        );
    }

    #[test]
    fn the_guard_sees_line_checks_and_spares_the_rest() {
        let flagged = [
            r#"if line.starts_with("ifdef::") {"#,
            r#"let rest = line.strip_prefix("include::")?;"#,
            r#"["include::", "ifdef::"].iter().any(|d| line.starts_with(d))"#,
            r#"rest.starts_with("tag::")"#,
            r#"line.strip_prefix("ifdef")"#,
            r#"let d = "\\ifdef::x[]";"#,
            // The pattern of the first `parse_directive`: keywords in a list.
            r#"["ifdef", "ifndef", "ifeval", "endif"].into_iter().find_map(|name| {"#,
            "let (name, rest) = line.split_once(\"::\")?;\nmatches!(name, \"ifdef\" | \"endif\")",
            "#[cfg(test)]\nfn helper() {}\nlet t = line.starts_with(\"ifdef::\");",
        ];
        for line in flagged {
            assert_eq!(line_checks(line).len(), 1, "{line}");
        }
        let spared = [
            r#"!matches!(name, "ifdef" | "ifndef" | "ifeval" | "endif")"#,
            r#"directive: "ifdef".into(),"#,
            r#"// line.starts_with("ifdef::")"#,
            r#"    /// `include::target[]`"#,
            "let x = line.starts_with(\"image::\");",
            "#[cfg(test)]\nmod tests {\nlet t = line.starts_with(\"ifdef::\");",
            "let (name, rest) = line.split_once(\"::\")?;\nmatches!(name, \"image\" | \"audio\")",
            r#"let kinds = ["ifdef", "section"];"#,
        ];
        for line in spared {
            assert!(line_checks(line).is_empty(), "{line}");
        }
    }
}
