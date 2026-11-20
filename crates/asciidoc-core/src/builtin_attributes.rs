//! Catalog of the built-in document attributes the server knows by name:
//! a description and, where one exists, the values the attribute takes.

/// What an attribute takes as its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinValues {
    /// A flag, or a value with no fixed shape.
    None,
    /// One of a fixed set of values.
    Choices(&'static [&'static str]),
    /// A free value, with an example of what it looks like.
    Example(&'static str),
}

/// A built-in document attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinAttribute {
    pub name: &'static str,
    pub description: &'static str,
    pub values: BuiltinValues,
}

impl BuiltinAttribute {
    /// The fixed values of the attribute; empty unless it takes a choice.
    #[must_use]
    pub fn choices(&self) -> &'static [&'static str] {
        match self.values {
            BuiltinValues::Choices(choices) => choices,
            BuiltinValues::None | BuiltinValues::Example(_) => &[],
        }
    }
}

const fn attr(
    name: &'static str,
    description: &'static str,
    values: BuiltinValues,
) -> BuiltinAttribute {
    BuiltinAttribute {
        name,
        description,
        values,
    }
}

/// The catalog, in the order completion offers it.
pub const BUILTIN_ATTRIBUTES: &[BuiltinAttribute] = &[
    attr(
        "toc",
        "Table of contents placement",
        BuiltinValues::Choices(&["auto", "left", "right", "preamble"]),
    ),
    attr(
        "icons",
        "Icon mode",
        BuiltinValues::Choices(&["font", "image"]),
    ),
    attr(
        "source-highlighter",
        "Source code highlighter",
        BuiltinValues::Choices(&["rouge", "highlight.js", "coderay"]),
    ),
    attr(
        "doctype",
        "Document type",
        BuiltinValues::Choices(&["article", "book", "manpage", "inline"]),
    ),
    attr(
        "imagesdir",
        "Images directory",
        BuiltinValues::Example("images"),
    ),
    attr(
        "numbered",
        "Auto-number sections (deprecated, use sectnums)",
        BuiltinValues::None,
    ),
    attr(
        "experimental",
        "Enable experimental features (kbd, btn, menu)",
        BuiltinValues::None,
    ),
    attr("data-uri", "Embed images as data URIs", BuiltinValues::None),
    attr("sectnums", "Number sections", BuiltinValues::None),
    attr(
        "sectanchors",
        "Add anchors to section titles",
        BuiltinValues::None,
    ),
    attr(
        "sectlinks",
        "Make section titles self-links",
        BuiltinValues::None,
    ),
    attr(
        "stem",
        "Enable STEM (math) support",
        BuiltinValues::Choices(&["latexmath", "asciimath"]),
    ),
    attr(
        "toclevels",
        "Number of ToC depth levels",
        BuiltinValues::Example("2"),
    ),
    attr(
        "hardbreaks-option",
        "Force hard line breaks",
        BuiltinValues::None,
    ),
];

/// The catalog attribute `name`, matched ASCII case-insensitively.
#[must_use]
pub fn builtin_attribute(name: &str) -> Option<&'static BuiltinAttribute> {
    BUILTIN_ATTRIBUTES
        .iter()
        .find(|attribute| attribute.name.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_choices_of_a_choice_attribute() {
        let toc = builtin_attribute("toc").unwrap();
        assert_eq!(toc.choices(), ["auto", "left", "right", "preamble"]);
    }

    #[test]
    fn test_no_choices_for_example_and_flag_attributes() {
        assert!(builtin_attribute("imagesdir").unwrap().choices().is_empty());
        assert!(builtin_attribute("numbered").unwrap().choices().is_empty());
    }

    #[test]
    fn test_lookup_ignores_ascii_case() {
        assert_eq!(builtin_attribute("TOC"), builtin_attribute("toc"));
        assert!(builtin_attribute("TOC").is_some());
    }

    #[test]
    fn test_lookup_unknown_name() {
        assert_eq!(builtin_attribute("myattr"), None);
        assert_eq!(builtin_attribute("pdf-theme"), None);
    }

    #[test]
    fn test_catalog_names_are_unique() {
        let mut names = std::collections::HashSet::new();
        for attribute in BUILTIN_ATTRIBUTES {
            assert!(names.insert(attribute.name), "duplicate {}", attribute.name);
        }
    }
}
