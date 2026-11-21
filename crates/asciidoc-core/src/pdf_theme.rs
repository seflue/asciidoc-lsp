//! Asciidoctor PDF path attributes: `pdf-theme`, `pdf-themesdir` and
//! `pdf-fontsdir`.
//!
//! Asciidoctor PDF reads these from the top-level document it converts, so an
//! entry in an included file (a shared `pdf-settings.adoc`) counts for every
//! document that includes it. Only the last entry in the header of each
//! attribute counts: Asciidoctor PDF loads the theme after the attributes are
//! reset to the end of the header, which `pdf_header` reads. A value is taken
//! after attribute substitution with what the header defined before it, else
//! with what the document starts with (`.asciidoctorconfig`, built-ins), and
//! `{docdir}` of the top-level document. It is checked once per such
//! document. A relative value resolves against the working directory of the
//! build, which the server cannot know.
//!
//! Hover reads the whole document instead: the last entry anywhere.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::attributes::{AttributeLayer, AttributeRegistry, AttributeSource};
use crate::graph::IncludeGraph;
use crate::paths::normalize_lexically;
use crate::pdf_header::{HeaderValue, PdfHeader};
use crate::pdf_theme_yaml::{check_theme, ThemeDirs};
use crate::types::{Diagnostic, DiagnosticKind, DocumentModel};
use crate::workspace::Workspace;

pub const PDF_THEME: &str = "pdf-theme";
pub const PDF_THEMESDIR: &str = "pdf-themesdir";
pub const PDF_FONTSDIR: &str = "pdf-fontsdir";

/// Themes bundled with Asciidoctor PDF (`data/themes/<name>-theme.yml`).
const BUILTIN_THEMES: &[&str] = &[
    "base",
    "default",
    "default-for-print",
    "default-for-print-with-fallback-font",
    "default-for-print-with-font-fallbacks",
    "default-sans",
    "default-sans-with-font-fallbacks",
    "default-with-fallback-font",
    "default-with-font-fallbacks",
];

/// `pdf-fontsdir` entry for the fonts bundled with Asciidoctor PDF.
pub(crate) const GEM_FONTS_DIR: &str = "GEM_FONTS_DIR";

/// Whether `name` is one of the PDF path attributes this module resolves.
#[must_use]
pub fn is_pdf_path_attribute(name: &str) -> bool {
    matches!(name, PDF_THEME | PDF_THEMESDIR | PDF_FONTSDIR)
}

/// Whether `name` is a theme bundled with Asciidoctor PDF.
#[must_use]
pub fn is_builtin_theme(name: &str) -> bool {
    BUILTIN_THEMES.contains(&name)
}

/// Where one PDF path value points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PdfPath {
    /// An absolute path, `..` popped.
    Absolute(PathBuf),
    /// A theme name or font directory looked up among Asciidoctor PDF's own.
    Builtin(String),
    /// A relative path: Asciidoctor PDF resolves it against the build's
    /// working directory.
    Relative(String),
    /// Not checkable: an attribute reference left unresolved, an empty or
    /// `.yaml` theme, a leading `~` (the home directory of the build), or a
    /// theme inside a themes directory that is itself not checkable.
    Unknown(String),
}

/// A PDF path attribute resolved for hover, with the entry that sets it.
#[derive(Debug, Clone)]
pub struct PdfResolution {
    pub source: Option<AttributeSource>,
    /// One path for `pdf-theme` and `pdf-themesdir`, one per list entry for
    /// `pdf-fontsdir`.
    pub paths: Vec<PdfPath>,
}

/// What a PDF path attribute is at the end of the header.
enum InEffect {
    Value(String),
    Unset,
    /// The header may or may not set it: nothing is checked against it.
    Uncertain,
}

/// The last header entry for `name`, else the value from the
/// `.asciidoctorconfig` chain. Entries of the file itself outside the header
/// do not count.
fn in_effect(
    header: &PdfHeader,
    registry: &AttributeRegistry,
    root: &Path,
    name: &str,
) -> InEffect {
    match header.value(name) {
        Some(HeaderValue::Set(entry)) => InEffect::Value(entry.value.clone()),
        Some(HeaderValue::Unset) => InEffect::Unset,
        Some(HeaderValue::Uncertain) => InEffect::Uncertain,
        None => match registry.preset(root, name) {
            Some((AttributeLayer::Config, hit)) => {
                InEffect::Value(registry.substitute(root, &hit.value))
            }
            Some((AttributeLayer::Builtin, _)) | None => InEffect::Unset,
        },
    }
}

/// The `pdf-themesdir` Asciidoctor PDF uses; `None` when unset.
fn themesdir(header: &PdfHeader, registry: &AttributeRegistry, root: &Path) -> Option<PdfPath> {
    match in_effect(header, registry, root, PDF_THEMESDIR) {
        InEffect::Value(value) => Some(resolve_dir(&value)),
        InEffect::Unset => None,
        InEffect::Uncertain => Some(PdfPath::Unknown(String::new())),
    }
}

/// The `pdf-fontsdir` entries Asciidoctor PDF uses; `None` when unset.
fn fontsdirs(
    header: &PdfHeader,
    registry: &AttributeRegistry,
    root: &Path,
) -> Option<Vec<PdfPath>> {
    match in_effect(header, registry, root, PDF_FONTSDIR) {
        InEffect::Value(value) => Some(resolve_fontsdir(&value)),
        InEffect::Unset => None,
        InEffect::Uncertain => Some(vec![PdfPath::Unknown(String::new())]),
    }
}

/// Documents no other document includes, sorted: the ones Asciidoctor PDF
/// would convert.
fn top_level_documents(
    documents: &HashMap<PathBuf, DocumentModel>,
    graph: &IncludeGraph,
) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = documents
        .keys()
        .filter(|path| graph.included_by(path).is_empty())
        .cloned()
        .collect();
    roots.sort();
    roots
}

/// Asciidoctor PDF tells a theme file from a theme name with Ruby's
/// case-sensitive `end_with? '.yml'`: `custom.YML` is a name.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn has_extension(value: &str, extension: &str) -> bool {
    value.ends_with(extension)
}

fn is_unresolved(value: &str) -> bool {
    value.is_empty() || value.contains('{')
}

/// Resolve a directory value the way Asciidoctor PDF's `File.expand_path` does.
fn resolve_dir(value: &str) -> PdfPath {
    // `File.expand_path` also expands a leading `~` (or `~user`) to a home directory.
    if is_unresolved(value) || value.starts_with('~') {
        PdfPath::Unknown(value.to_string())
    } else if Path::new(value).is_absolute() {
        PdfPath::Absolute(normalize_lexically(Path::new(value)))
    } else {
        PdfPath::Relative(value.to_string())
    }
}

/// Split `pdf-fontsdir` on `;` and `,` as Asciidoctor PDF does; an empty
/// entry or `GEM_FONTS_DIR` stands for the bundled fonts.
fn resolve_fontsdir(value: &str) -> Vec<PdfPath> {
    value
        .split([';', ','])
        .map(|dir| {
            if dir.is_empty() || dir == GEM_FONTS_DIR {
                PdfPath::Builtin(GEM_FONTS_DIR.to_string())
            } else {
                resolve_dir(dir)
            }
        })
        .collect()
}

/// Resolve `pdf-theme` as Asciidoctor PDF's `ThemeLoader.resolve_theme_file`:
/// a value ending in `.yml` is a path, relative to the themes directory when
/// one is set; anything else is a name, `<name>-theme.yml` in the themes
/// directory, or a bundled theme without one.
fn resolve_theme(value: &str, themesdir: Option<&PdfPath>) -> PdfPath {
    if is_unresolved(value) || has_extension(value, ".yaml") {
        return PdfPath::Unknown(value.to_string());
    }
    let is_file = has_extension(value, ".yml");
    match themesdir {
        None if is_file => resolve_dir(value),
        None => PdfPath::Builtin(value.to_string()),
        Some(PdfPath::Absolute(dir)) => {
            let file = if is_file {
                dir.join(value)
            } else {
                dir.join(format!("{value}-theme.yml"))
            };
            PdfPath::Absolute(normalize_lexically(&file))
        }
        Some(_) if is_file && Path::new(value).is_absolute() => resolve_dir(value),
        Some(_) => PdfPath::Unknown(value.to_string()),
    }
}

fn resolve_entry(name: &str, value: &str, themesdir: Option<&PdfPath>) -> Vec<PdfPath> {
    match name {
        PDF_THEME => vec![resolve_theme(value, themesdir)],
        PDF_FONTSDIR => resolve_fontsdir(value),
        _ => vec![resolve_dir(value)],
    }
}

/// The directories a theme's own paths resolve against.
struct Dirs {
    themesdir: Option<PdfPath>,
    fontsdirs: Option<Vec<PdfPath>>,
}

fn check_entry(name: &str, value: &str, dirs: &Dirs) -> Vec<DiagnosticKind> {
    let themesdir = dirs.themesdir.as_ref();
    if name == PDF_THEME && has_extension(value, ".yaml") {
        return vec![DiagnosticKind::PdfThemeYamlExtension {
            value: value.to_string(),
        }];
    }
    resolve_entry(name, value, themesdir)
        .into_iter()
        .flat_map(|path| match path {
            PdfPath::Absolute(path) if name == PDF_THEME && path.is_file() => {
                theme_findings(&path, dirs)
            }
            PdfPath::Absolute(path) => {
                let exists = if name == PDF_THEME {
                    false
                } else {
                    path.is_dir()
                };
                (!exists)
                    .then(|| DiagnosticKind::PdfPathNotFound {
                        attribute: name.to_string(),
                        path,
                    })
                    .into_iter()
                    .collect()
            }
            PdfPath::Relative(value) => vec![DiagnosticKind::UnanchoredPdfPath {
                attribute: name.to_string(),
                value,
            }],
            PdfPath::Builtin(theme) if name == PDF_THEME && !is_builtin_theme(&theme) => {
                vec![DiagnosticKind::UnknownPdfTheme { name: theme }]
            }
            PdfPath::Builtin(_) | PdfPath::Unknown(_) => Vec::new(),
        })
        .collect()
}

/// Missing paths inside the theme file `theme`, which exists.
fn theme_findings(theme: &Path, dirs: &Dirs) -> Vec<DiagnosticKind> {
    let themes = match &dirs.themesdir {
        Some(PdfPath::Absolute(dir)) => Some(dir.as_path()),
        Some(_) => None,
        None => theme.parent(),
    };
    let found = check_theme(
        theme,
        &ThemeDirs {
            themes,
            fonts: dirs.fontsdirs.as_deref(),
        },
    );
    found
        .into_iter()
        .map(|f| DiagnosticKind::PdfThemeEntryNotFound {
            theme: theme.to_path_buf(),
            line: f.line,
            key: f.key,
            path: f.path,
            searched: f.searched,
        })
        .collect()
}

/// Diagnostics for the `pdf-theme`, `pdf-themesdir` and `pdf-fontsdir`
/// entries Asciidoctor PDF reads (the last header entry of each, unless it
/// unsets, see [`PdfHeader`]), checked once per top-level document that
/// reaches it. An entry reached from several documents reports each distinct
/// finding once, and nothing when it checks clean for at least one of them.
#[must_use]
pub fn collect_pdf_diagnostics(workspace: &Workspace) -> Vec<Diagnostic> {
    let registry = workspace.attributes();
    let mut diags: Vec<Diagnostic> = Vec::new();
    let mut clean: HashSet<(PathBuf, usize)> = HashSet::new();
    for root in top_level_documents(&workspace.documents, &workspace.include_graph) {
        let header = PdfHeader::read(workspace, &root);
        let dirs = Dirs {
            themesdir: themesdir(&header, registry, &root),
            fontsdirs: fontsdirs(&header, registry, &root),
        };
        for name in [PDF_THEMESDIR, PDF_THEME, PDF_FONTSDIR] {
            let Some(HeaderValue::Set(entry)) = header.value(name) else {
                continue;
            };
            let kinds = check_entry(name, &entry.value, &dirs);
            if kinds.is_empty() {
                clean.insert((entry.file.clone(), entry.line));
            }
            for kind in kinds {
                let seen = diags
                    .iter()
                    .any(|d| d.file == entry.file && d.line == entry.line && d.kind == kind);
                if !seen {
                    diags.push(Diagnostic {
                        file: entry.file.clone(),
                        line: entry.line,
                        span: None,
                        kind,
                    });
                }
            }
        }
    }
    diags.retain(|d| !clean.contains(&(d.file.clone(), d.line)));
    diags
}

/// Resolve a PDF path attribute as seen from `file`: the entry on `line`
/// when given (an attribute entry), else the last entry for `name` in the
/// first top-level document that reaches `file` (an attribute reference),
/// else the registry value (`.asciidoctorconfig`). Unlike the diagnostics,
/// hover reads the whole document, not only its header.
#[must_use]
pub fn resolve_pdf_attribute(
    workspace: &Workspace,
    file: &Path,
    name: &str,
    line: Option<usize>,
) -> Option<PdfResolution> {
    if !is_pdf_path_attribute(name) {
        return None;
    }
    let registry = workspace.attributes();
    let file = workspace.indexed_path(file);
    let scopes = workspace.scopes_at(&file, line.unwrap_or(0));
    let scope = scopes.first()?;
    let root = scope.document();
    let themesdir = themesdir(&PdfHeader::read(workspace, root), registry, root);

    let set = scope
        .definitions(name)
        .filter(|definition| definition.value.is_some());
    let entry = match line {
        Some(line) => set
            .filter(|definition| {
                definition
                    .source
                    .as_ref()
                    .is_some_and(|source| source.file == file && source.line == line)
            })
            .last(),
        None => set.last(),
    };
    let (value, source) = if let Some(entry) = entry {
        (entry.value.clone()?, entry.source.clone())
    } else {
        let value = registry.resolve(root, name)?;
        (
            registry.substitute(root, value),
            registry.resolve_source(root, name),
        )
    };
    Some(PdfResolution {
        source,
        paths: resolve_entry(name, &value, themesdir.as_ref()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::diagnostics_for;
    use crate::workspace::Workspace;
    use pretty_assertions::assert_eq;

    /// Committed fixture `tests/fixtures/pdf_theme`, scanned with itself as
    /// workspace root. Every document using `{docdir}` is top-level in the
    /// fixture directory or included from one there, so `{docdir}` is the
    /// fixture directory.
    fn fixture() -> (PathBuf, Workspace) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/pdf_theme")
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.refresh();
        (root, ws)
    }

    /// PDF diagnostics of `file`, as `(line, message)`, sorted by line.
    fn pdf_diags(ws: &Workspace, file: &Path) -> Vec<(usize, String)> {
        let mut diags: Vec<(usize, String)> = diagnostics_for(ws, file)
            .iter()
            .filter(|d| {
                matches!(
                    d.kind,
                    DiagnosticKind::PdfPathNotFound { .. }
                        | DiagnosticKind::PdfThemeEntryNotFound { .. }
                        | DiagnosticKind::PdfThemeYamlExtension { .. }
                        | DiagnosticKind::UnknownPdfTheme { .. }
                        | DiagnosticKind::UnanchoredPdfPath { .. }
                )
            })
            .map(|d| (d.line, d.kind.to_string()))
            .collect();
        diags.sort();
        diags
    }

    /// Committed fixture `tests/fixtures/pdf_theme_yaml`: themes whose YAML
    /// names files, scanned with itself as workspace root.
    fn yaml_fixture() -> (PathBuf, Workspace) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/pdf_theme_yaml")
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.refresh();
        (root, ws)
    }

    fn resolve(ws: &Workspace, file: &Path, name: &str, line: Option<usize>) -> Vec<PdfPath> {
        resolve_pdf_attribute(ws, file, name, line).unwrap().paths
    }

    /// Asciidoctor PDF loads the theme when conversion starts, after the
    /// attributes are restored to their state at the end of the header
    /// (`Document#restore_attributes`; `converter.rb` `convert_document` →
    /// `load_theme`). Only the last header value of each attribute counts:
    /// an overwritten entry, one in the body or in a file included from the
    /// body, and one an unset (`:name!:`) cancels are never read.
    #[test]
    fn only_the_last_header_value_of_each_pdf_attribute_is_checked() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let files = [
            ("override.adoc", "= O\n:pdf-theme: nosuch-a\n:pdf-theme: default\n\nBody.\n"),
            (
                "unset.adoc",
                "= U\n:pdf-themesdir: {docdir}/nothemes\n:pdf-theme: default\n:pdf-themesdir!:\n\nBody.\n",
            ),
            ("body.adoc", "= B\n\nBody.\n\n:pdf-theme: nosuch-b\n"),
            ("last.adoc", "= L\n:pdf-theme: default\n:pdf-theme: nosuch-c\n\nBody.\n"),
            ("untitled.adoc", ":pdf-theme: nosuch-d\n\n:pdf-theme: nosuch-e\n\nBody.\n"),
            ("header-include.adoc", "= H\ninclude::in-header.adoc[]\n\nBody.\n"),
            ("in-header.adoc", ":pdf-theme: nosuch-f\n"),
            ("body-include.adoc", "= BI\n\ninclude::in-body.adoc[]\n"),
            ("in-body.adoc", ":pdf-theme: nosuch-g\n"),
            (
                "shared-override.adoc",
                "= S\n:pdf-themesdir: {docdir}/other\ninclude::settings.adoc[]\n:pdf-theme: default\n:pdf-themesdir!:\n\nBody.\n",
            ),
            ("settings.adoc", ":pdf-themesdir: {docdir}/themes\n:pdf-theme: corp\n"),
        ];
        for (name, text) in files {
            std::fs::write(root.join(name), text).unwrap();
        }
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.refresh();

        let found: Vec<(&str, Vec<usize>)> = files
            .iter()
            .map(|(name, _)| {
                let lines = pdf_diags(&ws, &root.join(name)).into_iter().map(|d| d.0);
                (*name, lines.collect())
            })
            .filter(|(_, lines): &(&str, Vec<usize>)| !lines.is_empty())
            .collect();
        assert_eq!(
            found,
            vec![
                ("last.adoc", vec![3]),
                ("untitled.adoc", vec![3]),
                ("in-header.adoc", vec![1]),
            ]
        );
    }

    /// Files of the fixture directory with the lines of their PDF diagnostics,
    /// those without any left out.
    fn fixture_pdf_lines() -> Vec<(String, Vec<usize>)> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/pdf_header")
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.refresh();
        let mut found = Vec::new();
        for entry in std::fs::read_dir(&root).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !name.ends_with(".adoc") {
                continue;
            }
            let lines: Vec<usize> = pdf_diags(&ws, &path).into_iter().map(|d| d.0).collect();
            if !lines.is_empty() {
                found.push((name, lines));
            }
        }
        found.sort();
        found
    }

    fn expected(entries: &[(&str, &[usize])]) -> Vec<(String, Vec<usize>)> {
        let mut list: Vec<_> = entries
            .iter()
            .map(|(name, lines)| ((*name).to_string(), lines.to_vec()))
            .collect();
        list.sort();
        list
    }

    /// Fixtures in `tests/fixtures/pdf_header`: where the header ends decides
    /// which `pdf-theme` entry Asciidoctor PDF reads. Each fixture is checked
    /// against the oracle, Asciidoctor with the attributes restored after the
    /// header: `nosuch` reports at its entry, `nil` stays quiet. Only the
    /// fixtures with a report are listed; the known limitations are listed
    /// apart.
    #[test]
    fn pdf_header_fixtures_match_asciidoctor() {
        let oracle: &[(&str, &[usize])] = &[
            ("include-before-title.adoc", &[3]),
            ("anchor-before-title.adoc", &[3]),
            ("attrlist-before-title.adoc", &[3]),
            ("ifdef-in-header.adoc", &[3]),
            ("author-email-revision.adoc", &[4]),
            ("author-only.adoc", &[3]),
            ("author-as-entries.adoc", &[4]),
            ("author-email.adoc", &[3]),
            ("author-revision.adoc", &[4]),
            ("author-revnumber-only.adoc", &[4]),
            ("comments-in-header.adoc", &[6]),
            ("include-then-entry.adoc", &[3]),
            ("untitled-comment-include.adoc", &[3]),
            ("entry-before-title.adoc", &[1]),
            ("untitled-blank-between.adoc", &[3]),
            ("unset-then-set.adoc", &[3]),
            ("title-then-attrlist-section.adoc", &[2]),
            ("ifdef-before-title.adoc", &[2]),
            ("include-blockcomment-blank-included.adoc", &[6]),
            ("revision-continued-entry.adoc", &[6]),
            ("untitled-leading-blanks.adoc", &[3]),
            ("revision-oneline-ifdef.adoc", &[4]),
            ("oneline-ifdef-no-revision.adoc", &[2]),
            ("revision-include-in-header-included.adoc", &[1]),
            ("revision-blockcomment.adoc", &[7]),
            ("blockcomment-no-revision.adoc", &[5]),
            ("revision-invalid-entry.adoc", &[5]),
            ("title-paragraph-as-author.adoc", &[3]),
            ("continued-entry-no-revision.adoc", &[4]),
            ("include-blank-before-title.adoc", &[3]),
            ("include-trailing-blank-before-title.adoc", &[3]),
            ("blank-lines-before-title.adoc", &[4]),
            ("entry-blank-include-title.adoc", &[5]),
            ("three-levels-third-included.adoc", &[1]),
            ("nested-entry-after-inner-included.adoc", &[2]),
            ("sets-nosuch-theme-included.adoc", &[1]),
            ("relative-dotdot-second-included.adoc", &[1]),
            ("ifdef-own-attr.adoc", &[4]),
            ("ifdef-attr-from-include.adoc", &[4]),
            ("ifdef-any.adoc", &[4]),
            ("ifndef-after-unset.adoc", &[5]),
            ("ifdef-basebackend-html.adoc", &[3]),
            ("ifndef-basebackend-pdf.adoc", &[3]),
            ("nested-ifdef.adoc", &[7]),
            ("ifdef-opened-in-include.adoc", &[3]),
            ("untitled-include-trailing-blank.adoc", &[2]),
            ("untitled-anchor-then-entry.adoc", &[2]),
            ("untitled-blocktitle-then-entry.adoc", &[2]),
            ("untitled-comment-blank-entry.adoc", &[3]),
            ("author-then-colon-comma.adoc", &[4]),
            ("oneline-ifdef-include-included.adoc", &[1]),
            ("uppercase-name.adoc", &[2]),
            ("crlf.adoc", &[4]),
            ("bom-titled.adoc", &[2]),
            ("bom-untitled.adoc", &[1]),
        ];
        // Known limitation, false alarm: `include::include-tag-included.adoc[tag=t]`
        // reads the whole file, the oracle reads only the tagged lines and
        // stays `nil`.
        let false_alarms: &[(&str, &[usize])] = &[("include-tag-included.adoc", &[4])];
        // Known limitations, missed errors (oracle `nosuch`, the LSP is quiet):
        // - `ifndef::foo[]` holds unless the build sets `foo`; nothing says
        //   `foo` is unset and a build may pass `-a foo`, so the branch is
        //   unknown, also for an attribute set in the body;
        // - nothing in an `ifeval` branch is checked;
        // - a title in an include does not open the header;
        // - an unresolved include blinds the walk;
        // - an unterminated comment block in an include, closed by the
        //   parent, is uncertain.
        let missed: &[&str] = &[
            "ifndef-in-untitled.adoc",
            "revision-ifeval.adoc",
            "title-in-include.adoc",
            "title-in-include-included.adoc",
            "missing-include.adoc",
            "ifndef-attr-set-in-body.adoc",
            "unterminated-comment-in-include.adoc",
            "unterminated-comment-in-include-included.adoc",
        ];
        let all = [oracle, false_alarms].concat();
        // The premise of the CRLF entry: the fixture has CRLF line ends.
        let crlf =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/pdf_header/crlf.adoc");
        assert!(
            std::fs::read(&crlf)
                .unwrap()
                .windows(2)
                .any(|w| w == b"\r\n"),
            "crlf.adoc must contain CRLF line ends"
        );
        // The premise of the BOM entries: the fixtures start with a BOM.
        for name in ["bom-untitled.adoc", "bom-titled.adoc"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/pdf_header")
                .join(name);
            assert!(
                std::fs::read(&path).unwrap().starts_with(b"\xEF\xBB\xBF"),
                "{name} must start with a BOM"
            );
        }
        let found = fixture_pdf_lines();
        assert_eq!(found, expected(&all));
        for name in missed {
            assert!(found.iter().all(|(file, _)| file != name), "{name}");
        }
    }

    /// One settings file included from two top-level documents: the blank
    /// line in it ends the header of the titled document, so its first entry
    /// counts there, while the untitled document reads on to the last entry.
    /// The finding stays on the shared file.
    #[test]
    fn shared_header_include_is_read_per_top_level_document() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/pdf_header_shared_include")
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.refresh();
        let lines = |name: &str| -> Vec<usize> {
            pdf_diags(&ws, &root.join(name))
                .into_iter()
                .map(|d| d.0)
                .collect()
        };
        assert_eq!(lines("settings.adoc"), vec![1]);
        assert_eq!(lines("titled.adoc"), Vec::<usize>::new());
        assert_eq!(lines("untitled.adoc"), Vec::<usize>::new());
    }

    #[test]
    fn builtin_theme_list_matches_asciidoctor_pdf() {
        // data/themes in asciidoctor/asciidoctor-pdf, `-theme.yml` cut off.
        assert_eq!(BUILTIN_THEMES.len(), 9);
        assert!(is_builtin_theme("default"));
        assert!(is_builtin_theme("default-with-font-fallbacks"));
        assert!(!is_builtin_theme("default-with-fallback-fonts"));
    }

    #[test]
    fn settings_from_included_file_resolve_for_including_document() {
        let (root, ws) = fixture();
        assert_eq!(pdf_diags(&ws, &root.join("main.adoc")), vec![]);
        assert_eq!(
            pdf_diags(&ws, &root.join("shared/pdf-settings.adoc")),
            vec![]
        );
    }

    #[test]
    fn missing_directories_and_theme_file_are_errors() {
        let (root, ws) = fixture();
        assert_eq!(
            pdf_diags(&ws, &root.join("missing.adoc")),
            vec![
                (2, "pdf-themesdir: not found: missing-themes".to_string()),
                (3, "pdf-fontsdir: not found: missing-fonts".to_string()),
                (
                    4,
                    "pdf-theme: not found: missing-themes/custom-theme.yml".to_string()
                ),
            ]
        );
    }

    #[test]
    fn absolute_path_outside_root_is_shown_as_written() {
        let (root, ws) = fixture();
        assert_eq!(
            pdf_diags(&ws, &root.join("absolute.adoc")),
            vec![(
                2,
                "pdf-themesdir: not found: /nonexistent-review/themes".to_string()
            )]
        );
    }

    #[test]
    fn relative_paths_warn_and_theme_inside_them_is_not_checked() {
        let (root, ws) = fixture();
        let anchor = "resolves against the build's working directory; anchor it with {docdir}";
        assert_eq!(
            pdf_diags(&ws, &root.join("relative.adoc")),
            vec![
                (2, format!("pdf-themesdir: relative path 'theme' {anchor}")),
                (3, format!("pdf-fontsdir: relative path 'fonts' {anchor}")),
            ]
        );
    }

    /// Ruby's `File.expand_path` expands a leading `~` to the home directory,
    /// so such a value is neither relative nor checkable.
    #[test]
    fn tilde_prefix_is_not_a_relative_path() {
        let (root, ws) = fixture();
        assert_eq!(pdf_diags(&ws, &root.join("tilde.adoc")), vec![]);
        assert_eq!(resolve_dir("~/themes"), PdfPath::Unknown("~/themes".into()));
        assert_eq!(resolve_dir("~"), PdfPath::Unknown("~".into()));
        assert_eq!(
            resolve_dir("~root/themes"),
            PdfPath::Unknown("~root/themes".into())
        );
        assert_eq!(resolve_dir("a~b"), PdfPath::Relative("a~b".into()));
    }

    #[test]
    fn theme_without_themesdir_is_builtin_name_or_anchored_yml_path() {
        // One document per value: only the last header entry is checked.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("theme")).unwrap();
        std::fs::write(root.join("theme/custom-theme.yml"), "").unwrap();
        let values = [
            "default-sans",
            "corporate",
            "{docdir}/theme/custom-theme.yaml",
            "theme/custom-theme.yml",
            "{docdir}/theme/custom-theme.yml",
            "{docdir}/theme/none-theme.yml",
            "{undefined-dir}/custom-theme.yml",
            "",
        ];
        for (i, value) in values.iter().enumerate() {
            std::fs::write(
                root.join(format!("d{i}.adoc")),
                format!("= D\n:pdf-theme: {value}\n"),
            )
            .unwrap();
        }
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.refresh();

        let found: Vec<(usize, String)> = (0..values.len())
            .flat_map(|i| {
                pdf_diags(&ws, &root.join(format!("d{i}.adoc")))
                    .into_iter()
                    .map(move |(_, message)| (i, message))
            })
            .collect();
        assert_eq!(
            found,
            vec![
                (
                    1,
                    "pdf-theme: 'corporate' is not a built-in theme; set pdf-themesdir for a custom theme"
                        .to_string()
                ),
                (
                    2,
                    "pdf-theme: theme file must end in .yml, not .yaml: theme/custom-theme.yaml"
                        .to_string()
                ),
                (
                    3,
                    "pdf-theme: relative path 'theme/custom-theme.yml' resolves against the build's working directory; anchor it with {docdir}"
                        .to_string()
                ),
                (5, "pdf-theme: not found: theme/none-theme.yml".to_string()),
            ]
        );
    }

    #[test]
    fn finding_in_file_included_by_two_documents_is_reported_once() {
        let (root, ws) = fixture();
        assert_eq!(
            pdf_diags(&ws, &root.join("shared/relative-fonts.adoc")),
            vec![(
                1,
                "pdf-fontsdir: relative path 'fonts' resolves against the build's working directory; anchor it with {docdir}"
                    .to_string()
            )]
        );
    }

    #[test]
    fn themesdir_from_asciidoctorconfig_counts() {
        let (root, ws) = fixture();
        let doc = root.join("configured/doc.adoc");
        assert_eq!(pdf_diags(&ws, &doc), vec![]);
        assert_eq!(
            resolve(&ws, &doc, PDF_THEME, Some(2)),
            vec![PdfPath::Absolute(root.join("theme/custom-theme.yml"))]
        );
    }

    #[test]
    fn hover_resolution_of_entries_and_references() {
        let (root, ws) = fixture();
        let main = root.join("main.adoc");
        let shared = root.join("shared/pdf-settings.adoc");

        assert_eq!(
            resolve(&ws, &main, PDF_THEME, Some(2)),
            vec![PdfPath::Absolute(root.join("theme/custom-theme.yml"))]
        );
        assert_eq!(
            resolve(&ws, &shared, PDF_FONTSDIR, Some(2)),
            vec![
                PdfPath::Absolute(root.join("fonts")),
                PdfPath::Builtin(GEM_FONTS_DIR.into()),
            ]
        );
        // `{pdf-themesdir}` in main.adoc: set in the included file.
        let reference = resolve_pdf_attribute(&ws, &main, PDF_THEMESDIR, None).unwrap();
        assert_eq!(reference.paths, vec![PdfPath::Absolute(root.join("theme"))]);
        let source = reference.source.unwrap();
        assert_eq!((source.file, source.line), (shared, 1));
        assert_eq!(
            resolve(&ws, &root.join("no-themesdir.adoc"), PDF_THEME, Some(2)),
            vec![PdfPath::Builtin("default-sans".into())]
        );
    }

    /// `{docdir}` in a shared file is the folder of each top-level document
    /// that includes it (committed fixture `tests/fixtures/docdir_top_level`):
    /// `docs/handbook/handbook.adoc` finds `docs/theme`. For
    /// `docs/print/guide/guide.adoc` it points to the missing
    /// `docs/print/theme`, which is not reported: the entry resolves for one
    /// including document.
    #[test]
    fn docdir_in_shared_settings_is_folder_of_each_top_level_document() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/docdir_top_level")
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.refresh();
        let settings = root.join("docs/shared/pdf-settings.adoc");

        assert_eq!(
            pdf_diags(&ws, &root.join("docs/handbook/handbook.adoc")),
            vec![]
        );
        assert_eq!(pdf_diags(&ws, &settings), vec![]);
        // Hover: the first top-level document in sorted order.
        assert_eq!(
            resolve(&ws, &settings, PDF_THEMESDIR, Some(1)),
            vec![PdfPath::Absolute(root.join("docs/theme"))]
        );
        assert_eq!(
            resolve(
                &ws,
                &root.join("docs/handbook/handbook.adoc"),
                PDF_THEME,
                Some(3)
            ),
            vec![PdfPath::Absolute(root.join("docs/theme/custom-theme.yml"))]
        );
    }

    /// An attribute the top-level document defines before the include
    /// substitutes in the shared file (committed fixture
    /// `tests/fixtures/pdf_top_level_attrs`), with the value at the include
    /// line: `top.adoc` redefines `theme-root` after the include.
    #[test]
    fn attributes_of_top_level_document_substitute_in_shared_file() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/pdf_top_level_attrs")
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.refresh();
        let shared = root.join("shared/pdf.adoc");

        let missing_dir = root.join("theme");
        assert_eq!(
            pdf_diags(&ws, &shared),
            vec![
                (1, "pdf-themesdir: not found: theme".to_string()),
                (
                    2,
                    "pdf-theme: not found: theme/custom-theme.yml".to_string()
                ),
            ]
        );
        assert_eq!(
            resolve(&ws, &shared, PDF_THEMESDIR, Some(1)),
            vec![PdfPath::Absolute(missing_dir)]
        );
    }

    /// Diagnostics of the shared file of a committed top-level fixture.
    fn shared_diags(fixture: &str) -> Vec<(usize, String)> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(fixture)
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.refresh();
        pdf_diags(&ws, &root.join("shared/pdf.adoc"))
    }

    /// `:theme-root!:` before the include leaves `{theme-root}` unresolved
    /// in the shared file, as in Asciidoctor: nothing to check, no report
    /// (fixture `tests/fixtures/pdf_top_level_unset`).
    #[test]
    fn unset_in_top_level_document_leaves_reference_unresolved() {
        assert_eq!(shared_diags("pdf_top_level_unset"), vec![]);
    }

    /// An entry in a block conditional may not run, so `{theme-root}` is
    /// not known at the include (fixture
    /// `tests/fixtures/pdf_top_level_conditional`).
    #[test]
    fn conditional_entry_in_top_level_document_is_not_certain() {
        assert_eq!(shared_diags("pdf_top_level_conditional"), vec![]);
    }

    #[test]
    fn theme_yaml_with_existing_paths_has_no_findings() {
        let (root, ws) = yaml_fixture();
        assert_eq!(pdf_diags(&ws, &root.join("ok.adoc")), vec![]);
    }

    /// Findings sit on the `:pdf-theme:` line of the document, one per
    /// missing path, naming the theme file and the line in it. `extends`
    /// and relative images resolve against the themes directory, a font
    /// against `pdf-fontsdir` where `GEM_FONTS_DIR` stands for the bundled
    /// fonts. Remote and `data:` images are not files.
    #[test]
    fn theme_yaml_reports_missing_paths_on_the_pdf_theme_line() {
        let (root, ws) = yaml_fixture();
        let mut expected: Vec<(usize, String)> = [
            "3: extends: not found: themes/missing-parent-theme.yml",
            "4: extends: not found: themes/also-missing.yml",
            "6: title-page.background-image: not found: themes/missing-background.png",
            "17: font.catalog.Custom.bold: not found: custom-bold.ttf (searched: fonts, GEM_FONTS_DIR)",
            "19: font.catalog.Absolute.normal: not found: /nonexistent-yaml/absolute.ttf",
            "21: font.catalog.Bundled.normal: not found: GEM_FONTS_DIR/nope.ttf (searched: GEM_FONTS_DIR)",
        ]
        .iter()
        .map(|m| (4, format!("pdf-theme: themes/broken-theme.yml:{m}")))
        .collect();
        expected.sort();
        assert_eq!(pdf_diags(&ws, &root.join("broken.adoc")), expected);
    }

    /// `./` in `extends` is relative to the theme file's folder, any other
    /// `extends` and images to the themes directory, here a different one.
    #[test]
    fn theme_yaml_separates_themes_directory_from_theme_folder() {
        let (root, ws) = yaml_fixture();
        assert_eq!(
            pdf_diags(&ws, &root.join("shifted.adoc")),
            vec![(
                3,
                "pdf-theme: other/shifted-theme.yml:9: header.recto.left.content: not found: themes/other-only.png"
                    .to_string()
            )]
        );
    }

    /// Without `pdf-themesdir` a theme in another folder resolves against
    /// that folder.
    #[test]
    fn theme_yaml_without_themesdir_resolves_against_its_own_folder() {
        let (root, ws) = yaml_fixture();
        assert_eq!(
            pdf_diags(&ws, &root.join("ownfolder.adoc")),
            vec![(
                2,
                "pdf-theme: other/own-theme.yml:5: title-page.background-image: not found: other/missing-own.png"
                    .to_string()
            )]
        );
    }

    /// Without `pdf-themesdir` the theme file's folder is the themes
    /// directory; without `pdf-fontsdir` fonts are looked up there, then
    /// among the bundled ones.
    #[test]
    fn theme_yaml_without_directories_uses_theme_folder_and_bundled_fonts() {
        let (root, ws) = yaml_fixture();
        assert_eq!(
            pdf_diags(&ws, &root.join("nodirs.adoc")),
            vec![(
                2,
                "pdf-theme: themes/nodirs-theme.yml:8: font.catalog.Custom.normal: not found: custom-regular.ttf (searched: themes, GEM_FONTS_DIR)"
                    .to_string()
            )]
        );
    }

    /// Without `pdf-fontsdir` fonts are looked up in the themes directory
    /// (`theme.__dir__`), which is not the theme file's folder when
    /// `pdf-themesdir` is set.
    #[test]
    fn theme_yaml_fonts_without_fontsdir_use_the_themes_directory() {
        let (root, ws) = yaml_fixture();
        assert_eq!(
            pdf_diags(&ws, &root.join("fontshift.adoc")),
            vec![(
                3,
                "pdf-theme: other/fontshift-theme.yml:6: font.catalog.Own.normal: not found: other-font.ttf (searched: themes, GEM_FONTS_DIR)"
                    .to_string()
            )]
        );
    }

    /// Only the `.asciidoctorconfig` chain presets a name the header does
    /// not touch. An entry after the header, or in a branch the header
    /// skips, is not read by Asciidoctor PDF (oracle: no themesdir, the
    /// built-in theme applies).
    #[test]
    fn entries_outside_the_header_do_not_preset_directories() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/pdf_theme_body_entries")
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.refresh();
        for name in [
            "body-themesdir.adoc",
            "skipped-themesdir.adoc",
            "body-fontsdir.adoc",
        ] {
            assert_eq!(pdf_diags(&ws, &root.join(name)), vec![], "{name}");
        }
    }

    /// Hover reads the whole document through the attribute scope, where an
    /// entry inside a conditional counts as set. Diagnostics read only the
    /// header and treat that entry as uncertain.
    #[test]
    fn hover_counts_entries_in_conditionals_as_set() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/pdf_theme_body_entries")
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.refresh();
        let doc = root.join("hover-conditional.adoc");
        let expected = vec![PdfPath::Absolute(PathBuf::from("/custom/themes"))];
        assert_eq!(resolve(&ws, &doc, PDF_THEMESDIR, Some(4)), expected);
        assert_eq!(resolve(&ws, &doc, PDF_THEMESDIR, None), expected);
        assert_eq!(pdf_diags(&ws, &doc), vec![]);
    }

    /// A reference to an entry in a conditional is substituted for hover
    /// too, the attribute counts as set there.
    #[test]
    fn hover_substitutes_references_to_entries_in_conditionals() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/pdf_theme_body_entries")
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.refresh();
        let doc = root.join("hover-conditional-reference.adoc");
        let expected = vec![PdfPath::Absolute(PathBuf::from("/custom/themes"))];
        assert_eq!(resolve(&ws, &doc, PDF_THEMESDIR, Some(6)), expected);
        assert_eq!(resolve(&ws, &doc, PDF_THEMESDIR, None), expected);
        assert_eq!(pdf_diags(&ws, &doc), vec![]);
    }

    #[test]
    fn resolution_rules() {
        let dir = PdfPath::Absolute(PathBuf::from("/themes"));
        let relative = PdfPath::Relative("themes".into());
        for (value, themesdir, expected) in [
            (
                "custom",
                Some(&dir),
                PdfPath::Absolute("/themes/custom-theme.yml".into()),
            ),
            (
                "sub/x.yml",
                Some(&dir),
                PdfPath::Absolute("/themes/sub/x.yml".into()),
            ),
            (
                "/abs/x.yml",
                Some(&dir),
                PdfPath::Absolute("/abs/x.yml".into()),
            ),
            (
                "/abs/x.yml",
                Some(&relative),
                PdfPath::Absolute("/abs/x.yml".into()),
            ),
            ("custom", Some(&relative), PdfPath::Unknown("custom".into())),
            ("x.yml", Some(&relative), PdfPath::Unknown("x.yml".into())),
            ("x.yaml", None, PdfPath::Unknown("x.yaml".into())),
            // Ruby expands a leading `~` only for a lone theme file, never
            // inside a themes directory (`File.absolute_path`).
            (
                "~/custom.yml",
                None,
                PdfPath::Unknown("~/custom.yml".into()),
            ),
            (
                "~/x.yml",
                Some(&dir),
                PdfPath::Absolute("/themes/~/x.yml".into()),
            ),
            // Case-sensitive like Ruby's `end_with?`: a name, not a file.
            ("custom.YML", None, PdfPath::Builtin("custom.YML".into())),
            ("", None, PdfPath::Unknown(String::new())),
            (
                "{nope}/x.yml",
                None,
                PdfPath::Unknown("{nope}/x.yml".into()),
            ),
        ] {
            assert_eq!(resolve_theme(value, themesdir), expected, "{value:?}");
        }
        assert_eq!(
            resolve_fontsdir("/a;,GEM_FONTS_DIR, b"),
            vec![
                PdfPath::Absolute("/a".into()),
                PdfPath::Builtin(GEM_FONTS_DIR.into()),
                PdfPath::Builtin(GEM_FONTS_DIR.into()),
                // Asciidoctor PDF does not trim list entries.
                PdfPath::Relative(" b".into()),
            ]
        );
    }

    /// A path outside the workspace root stays absolute, also one folder
    /// above it, whether the user wrote it literally or through
    /// `{docdir}/..`. Only paths under the root are shown root-relative.
    #[test]
    fn path_outside_root_stays_absolute_even_one_folder_away() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let root = base.join("root");
        std::fs::create_dir_all(&root).unwrap();
        let literal = base.join("nothemes");
        let via_docdir = base.join("nofonts");
        let inside = root.join("nodir");
        let doc = root.join("doc.adoc");
        std::fs::write(
            &doc,
            format!(
                "= Doc\n:pdf-themesdir: {}\n:pdf-fontsdir: {{docdir}}/../nofonts\n:pdf-theme: {}\n",
                literal.display(),
                inside.join("t.yaml").display()
            ),
        )
        .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.refresh();
        let messages: Vec<String> = pdf_diags(&ws, &doc).into_iter().map(|(_, m)| m).collect();
        for expected in [
            format!("not found: {}", literal.display()),
            format!("not found: {}", via_docdir.display()),
            "not .yaml: nodir/t.yaml".to_string(),
        ] {
            assert!(
                messages.iter().any(|m| m.ends_with(&expected)),
                "missing {expected:?} in {messages:?}"
            );
        }
    }

    /// An absolute value that shares a real ancestor with a deep workspace
    /// root is shown exactly as the user wrote it, not as a longer `../` chain.
    #[test]
    fn absolute_value_sharing_ancestor_with_deep_root_stays_as_written() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let root = base.join("d1/d2/d3/d4/d5/d6/d7/d8");
        std::fs::create_dir_all(&root).unwrap();
        let themes = base.join("n-themes");
        let theme_yaml = base.join("n.yaml");
        let doc = root.join("doc.adoc");
        std::fs::write(
            &doc,
            format!(
                "= Doc\n:pdf-themesdir: {}\n:pdf-theme: {}\n",
                themes.display(),
                theme_yaml.display()
            ),
        )
        .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.refresh();
        let messages: Vec<String> = pdf_diags(&ws, &doc).into_iter().map(|(_, m)| m).collect();
        assert!(
            messages
                .iter()
                .any(|m| m.ends_with(&format!("not found: {}", themes.display()))),
            "got: {messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|m| m.ends_with(&format!("not .yaml: {}", theme_yaml.display()))),
            "got: {messages:?}"
        );
    }
}
