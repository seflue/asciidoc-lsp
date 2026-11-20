//! Paths inside an Asciidoctor PDF theme file: `extends`, `image:` values
//! and `font.catalog` font files.
//!
//! Asciidoctor PDF resolves `extends` and images against the themes
//! directory and fonts against `pdf-fontsdir`. A value the server cannot
//! resolve (attribute or theme variable references, a relative directory)
//! is not checked. The YAML loader expands an alias into a copy of its
//! anchor; a missing path inside an anchor is reported once, at the
//! anchor's line. A merge key `<<` counts as an ordinary key.

use std::path::{Path, PathBuf};

use saphyr::{LoadableYamlNode, MarkedYaml};

use crate::paths::normalize_lexically;
use crate::pdf_theme::{is_builtin_theme, PdfPath, GEM_FONTS_DIR};

/// Font files bundled with Asciidoctor PDF (`data/fonts`).
const BUILTIN_FONTS: &[&str] = &[
    "mplus1mn-bold-subset.ttf",
    "mplus1mn-bold_italic-subset.ttf",
    "mplus1mn-italic-subset.ttf",
    "mplus1mn-regular-subset.ttf",
    "mplus1p-regular-fallback.ttf",
    "notoemoji-subset.ttf",
    "notosans-bold-subset.ttf",
    "notosans-bold_italic-subset.ttf",
    "notosans-italic-subset.ttf",
    "notosans-regular-subset.ttf",
    "notoserif-bold-subset.ttf",
    "notoserif-bold_italic-subset.ttf",
    "notoserif-italic-subset.ttf",
    "notoserif-regular-subset.ttf",
];

/// A path in a theme file that does not exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeFinding {
    /// 1-based line of the value in the theme file.
    pub line: usize,
    /// Dotted key path, e.g. `font.catalog.Custom.bold`.
    pub key: String,
    /// The missing file: absolute for `extends` and images, as written for
    /// a font looked up in directories.
    pub path: PathBuf,
    /// Font directories looked in; empty for other entries.
    pub searched: Vec<String>,
}

/// What a theme's paths resolve against.
pub struct ThemeDirs<'a> {
    /// The themes directory (`pdf-themesdir`, else the theme file's folder);
    /// `None` when it is a relative path the server cannot anchor.
    pub themes: Option<&'a Path>,
    /// `pdf-fontsdir` entries; `None` when the attribute is not set, which
    /// means the themes directory, then the bundled fonts.
    pub fonts: Option<&'a [PdfPath]>,
}

/// The missing paths in the theme file at `theme`. A file that cannot be
/// read or parsed has none: YAML errors belong to the YAML buffer.
#[must_use]
pub fn check_theme(theme: &Path, dirs: &ThemeDirs<'_>) -> Vec<ThemeFinding> {
    let Ok(source) = std::fs::read_to_string(theme) else {
        return Vec::new();
    };
    let Ok(docs) = MarkedYaml::load_from_str(&source) else {
        return Vec::new();
    };
    let Some(root) = docs.first() else {
        return Vec::new();
    };
    let theme_folder = theme.parent().unwrap_or_else(|| Path::new("/"));
    let mut findings: Vec<ThemeFinding> = Vec::new();
    visit(root, &mut Vec::new(), &mut |key, value, line, col| {
        let Some(finding) = check_value(key, value, line, theme_folder, dirs) else {
            return;
        };
        // An alias of a scalar anchor sits at the alias (`*name`), not at its
        // anchor: the path is reported once, where the first check found it.
        // An alias of a mapping anchor repeats the anchor's values at their
        // own lines.
        let repeated = if is_alias_at(&source, line, col) {
            findings.iter().any(|f| f.path == finding.path)
        } else {
            findings
                .iter()
                .any(|f| f.line == finding.line && f.path == finding.path)
        };
        if !repeated {
            findings.push(finding);
        }
    });
    findings.sort_by_key(|f| f.line);
    findings
}

/// Call `f` with the key path, text, line and column of every string
/// scalar. A sequence adds nothing to the key path.
fn visit(
    node: &MarkedYaml<'_>,
    key: &mut Vec<String>,
    f: &mut impl FnMut(&[String], &str, usize, usize),
) {
    if let Some(text) = node.data.as_str() {
        f(key, text, node.span.start.line(), node.span.start.col());
    } else if let Some(mapping) = node.data.as_mapping() {
        for (name, value) in mapping {
            key.push(name.data.as_str().unwrap_or_default().to_string());
            visit(value, key, f);
            key.pop();
        }
    } else if let Some(items) = node.data.as_sequence() {
        for item in items {
            visit(item, key, f);
        }
    }
}

fn check_value(
    key: &[String],
    value: &str,
    line: usize,
    theme_folder: &Path,
    dirs: &ThemeDirs<'_>,
) -> Option<ThemeFinding> {
    let dotted = key.join(".");
    let missing = |path: PathBuf, searched: Vec<String>| ThemeFinding {
        line,
        key: dotted.clone(),
        path,
        searched,
    };
    match key {
        [only] if only == "extends" && !is_unresolvable(value) => {
            let path = extends_path(value, theme_folder, dirs.themes)?;
            (!path.is_file()).then(|| missing(path, Vec::new()))
        }
        [font, catalog, _, _]
            if font == "font" && catalog == "catalog" && !is_unresolvable(value) =>
        {
            check_font(value, dirs.fonts, dirs.themes)
                .map(|(path, searched)| missing(path, searched))
        }
        [only] if only == "extends" => None,
        [font, catalog, _, _] if font == "font" && catalog == "catalog" => None,
        _ => {
            let target = image_target(value)?;
            let path = if Path::new(target).is_absolute() {
                PathBuf::from(target)
            } else {
                dirs.themes?.join(target)
            };
            let path = normalize_lexically(&path);
            (!path.is_file()).then(|| missing(path, Vec::new()))
        }
    }
}

/// Whether the node at 1-based `line`, 0-based `col` of `source` is an
/// alias (`*name`).
fn is_alias_at(source: &str, line: usize, col: usize) -> bool {
    source
        .lines()
        .nth(line.saturating_sub(1))
        .and_then(|text| text.chars().nth(col))
        == Some('*')
}

/// An attribute or theme variable reference: the value is not known here.
fn is_unresolvable(value: &str) -> bool {
    value.contains('{') || value.contains('$')
}

/// The file of an `extends` entry as Asciidoctor PDF's `ThemeLoader` resolves
/// it; `None` for `base`, a bundled theme or an unanchored directory.
fn extends_path(value: &str, theme_folder: &Path, themes: Option<&Path>) -> Option<PathBuf> {
    let name = value.strip_suffix(" !important").unwrap_or(value);
    if is_builtin_theme(name) {
        return None;
    }
    let dir = if name.starts_with("./") {
        theme_folder
    } else {
        themes?
    };
    let file = if has_yml_extension(name) {
        dir.join(name)
    } else {
        dir.join(format!("{name}-theme.yml"))
    };
    Some(normalize_lexically(&file))
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn has_yml_extension(name: &str) -> bool {
    name.ends_with(".yml")
}

/// The target of a value that is one `image:target[attrs]` macro, as
/// Asciidoctor PDF's `ImageAttributeValueRx` reads it; `None` for a URL, a
/// `data:` URI or anything else that is not a local file.
fn image_target(value: &str) -> Option<&str> {
    let rest = value.strip_prefix("image:")?;
    let rest = rest.strip_prefix(':').unwrap_or(rest);
    let (target, attrs) = rest.split_once('[')?;
    if !attrs.ends_with(']') || target.is_empty() || is_unresolvable(target) || target.contains(':')
    {
        return None;
    }
    Some(target)
}

/// A font file not found where Asciidoctor PDF looks, with the directories
/// it looked in. `None` when the file is there or a directory is unknown.
fn check_font(
    value: &str,
    fonts: Option<&[PdfPath]>,
    themes: Option<&Path>,
) -> Option<(PathBuf, Vec<String>)> {
    if Path::new(value).is_absolute() {
        let path = normalize_lexically(Path::new(value));
        return (!path.is_file()).then_some((path, Vec::new()));
    }
    // Asciidoctor PDF replaces the prefix and the one character after it
    // (`theme_loader.rb`, `path[13]`) with the bundled fonts directory.
    if let Some(rest) = value.strip_prefix(GEM_FONTS_DIR) {
        let mut chars = rest.chars();
        if chars.next().is_some() {
            return (!BUILTIN_FONTS.contains(&chars.as_str()))
                .then(|| (PathBuf::from(value), vec![GEM_FONTS_DIR.to_string()]));
        }
    }
    // Without `pdf-fontsdir`: the themes directory, then the bundled fonts.
    let default;
    let dirs = if let Some(dirs) = fonts {
        dirs
    } else {
        default = [
            PdfPath::Absolute(themes?.to_path_buf()),
            PdfPath::Builtin(GEM_FONTS_DIR.to_string()),
        ];
        &default
    };
    let mut searched = Vec::new();
    for dir in dirs {
        match dir {
            PdfPath::Absolute(dir) => {
                if normalize_lexically(&dir.join(value)).is_file() {
                    return None;
                }
                searched.push(dir.display().to_string());
            }
            PdfPath::Builtin(_) => {
                if BUILTIN_FONTS.contains(&value) {
                    return None;
                }
                searched.push(GEM_FONTS_DIR.to_string());
            }
            PdfPath::Relative(_) | PdfPath::Unknown(_) => return None,
        }
    }
    Some((PathBuf::from(value), searched))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn findings(yaml: &str) -> Vec<(usize, String)> {
        let tmp = tempfile::tempdir().unwrap();
        let theme = tmp.path().join("t-theme.yml");
        std::fs::write(&theme, yaml).unwrap();
        check_theme(
            &theme,
            &ThemeDirs {
                themes: Some(tmp.path()),
                fonts: None,
            },
        )
        .into_iter()
        .map(|f| (f.line, f.key))
        .collect()
    }

    /// The loader expands aliases, so a missing path in an anchor is seen
    /// once per use, all at the anchor's line; it is reported once, under
    /// the anchor's key. `<<` is a plain key.
    #[test]
    fn aliases_do_not_repeat_findings_of_their_anchor_and_merge_keys_are_plain_keys() {
        let yaml = "base: &b\n  logo: image:in-anchor.png[]\nheading:\n  <<: *b\n  logo: image:missing.png[]\nalias: *b\n";
        assert_eq!(
            findings(yaml),
            vec![
                (2, "base.logo".to_string()),
                (5, "heading.logo".to_string()),
            ]
        );
    }

    /// An alias of a scalar anchor carries the alias's own position, not the
    /// anchor's: the missing path is still reported once, at the anchor.
    #[test]
    fn aliases_of_a_scalar_anchor_do_not_repeat_its_finding() {
        let yaml = "shared: &img image:dup.png[]\nheader:\n  bg: *img\nfooter:\n  bg: *img\n";
        assert_eq!(findings(yaml), vec![(1, "shared".to_string())]);
    }

    /// The column of an alias counts characters: a key with a non-ASCII
    /// character in front of it must not turn the alias into a second finding.
    #[test]
    fn alias_after_a_non_ascii_key_is_still_an_alias() {
        let yaml = "shared: &img image:dup.png[]\nfooter:\n  größe_bild: *img\n";
        assert_eq!(findings(yaml), vec![(1, "shared".to_string())]);
    }

    /// An alias is checked under its own key: an anchor outside the font
    /// catalog or `extends` reports nothing there, so the alias must. The
    /// path stays reported once where an anchor was checked the same way.
    #[test]
    fn alias_is_checked_where_its_anchor_is_not() {
        let yaml = "base_font: &f missing-x.ttf\nfont:\n  catalog:\n    Body:\n      normal: *f\n";
        assert_eq!(
            findings(yaml),
            vec![(5, "font.catalog.Body.normal".to_string())]
        );
        let yaml = "parent: &p nosuch.yml\nextends: *p\n";
        assert_eq!(findings(yaml), vec![(2, "extends".to_string())]);
        let yaml =
            "font:\n  catalog:\n    A:\n      normal: &f missing.ttf\n    B:\n      normal: *f\n";
        assert_eq!(
            findings(yaml),
            vec![(4, "font.catalog.A.normal".to_string())]
        );
    }

    #[test]
    fn invalid_yaml_has_no_findings() {
        assert_eq!(findings("extends: [unclosed\n  - x: : :\n"), vec![]);
    }

    #[test]
    fn values_with_references_or_urls_are_not_checked() {
        let yaml = "a: image:$logo[]\nb: image:{themesdir}/x.png[]\nc: image:https://e.org/x.png[]\nd: image:data:image/png;base64,AA[]\ne: image:\nextends: '{x}'\n";
        assert_eq!(findings(yaml), vec![]);
    }
}
