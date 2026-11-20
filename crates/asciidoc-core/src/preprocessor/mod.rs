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
//! Not in this module: the header lines (`extract_header_lines`, which only
//! asks [`directive`]) and the reading order over includes (`reading`), which
//! work on what the layer recognized.

mod conditionals;
mod directive;
mod tags;

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

    /// The code lines of `text` that test a line for a directive: a string
    /// literal `"<keyword>::` (the start of a directive or a tag directive),
    /// or a keyword handed to a line test (`starts_with("ifdef")`). Comment
    /// lines and the unit tests at the bottom of a file do not count, nor do
    /// names of grammar nodes (`"ifdef" | "endif"` in a `matches!`).
    fn line_checks(text: &str) -> Vec<(usize, &str)> {
        let mut needles = Vec::new();
        for keyword in KEYWORDS {
            needles.push(format!("\"{keyword}::"));
            needles.push(format!("\"\\\\{keyword}::"));
            for test in LINE_TESTS {
                needles.push(format!(".{test}(\"{keyword}\""));
            }
        }
        text.lines()
            .enumerate()
            .take_while(|(_, line)| line.trim() != "#[cfg(test)]")
            .filter(|(_, line)| !line.trim_start().starts_with("//"))
            .filter(|(_, line)| needles.iter().any(|needle| line.contains(needle.as_str())))
            .map(|(index, line)| (index + 1, line))
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
            "#[cfg(test)]\nlet t = line.starts_with(\"ifdef::\");",
        ];
        for line in spared {
            assert!(line_checks(line).is_empty(), "{line}");
        }
    }
}
