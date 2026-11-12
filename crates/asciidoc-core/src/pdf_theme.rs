//! Asciidoctor PDF path attributes: `pdf-theme`, `pdf-themesdir` and
//! `pdf-fontsdir`.
//!
//! Asciidoctor PDF reads these from the top-level document it converts, so an
//! entry in an included file (a shared `pdf-settings.adoc`) counts for every
//! document that includes it. A value is taken after attribute substitution
//! in the file that sets it, with `{docdir}` of the top-level document that
//! includes it, checked once per such document. A relative value resolves against the working
//! directory of the build, which the server cannot know.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::attributes::{AttributeRegistry, AttributeSource, DOCDIR};
use crate::graph::IncludeGraph;
use crate::paths::normalize_lexically;
use crate::types::{AttributeDef, Diagnostic, DiagnosticKind, DocumentModel};

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
const GEM_FONTS_DIR: &str = "GEM_FONTS_DIR";

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
    /// `.yaml` theme, or a theme inside a themes directory that is itself
    /// not checkable.
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

/// An attribute entry for a PDF path attribute, value substituted.
struct Entry<'a> {
    file: &'a Path,
    line: usize,
    name: &'a str,
    value: String,
}

/// The PDF path entries one top-level document sees, in document order.
struct Scope<'a> {
    root: &'a Path,
    /// `{docdir}` of `root`, which every file it includes sees.
    docdir: String,
    entries: Vec<Entry<'a>>,
    files: HashSet<&'a Path>,
}

/// One step of a document in line order: an attribute entry or an include.
enum Step<'a> {
    Attribute(&'a AttributeDef),
    Include(&'a Path),
}

impl<'a> Scope<'a> {
    fn collect(
        root: &'a Path,
        documents: &'a HashMap<PathBuf, DocumentModel>,
        graph: &'a IncludeGraph,
        registry: &AttributeRegistry,
    ) -> Self {
        let mut scope = Scope {
            root,
            docdir: registry
                .resolve(root, DOCDIR)
                .unwrap_or_default()
                .to_string(),
            entries: Vec::new(),
            files: HashSet::new(),
        };
        scope.visit(root, documents, graph, registry);
        scope
    }

    fn visit(
        &mut self,
        file: &'a Path,
        documents: &'a HashMap<PathBuf, DocumentModel>,
        graph: &'a IncludeGraph,
        registry: &AttributeRegistry,
    ) {
        let Some(doc) = documents.get(file) else {
            return;
        };
        if !self.files.insert(doc.path.as_path()) {
            return;
        }
        let mut steps: Vec<(usize, Step<'a>)> = doc
            .attributes
            .iter()
            .filter(|attr| is_pdf_path_attribute(&attr.name))
            .map(|attr| (attr.line, Step::Attribute(attr)))
            .chain(
                graph
                    .includes_of(&doc.path)
                    .iter()
                    .map(|edge| (edge.line, Step::Include(edge.target.as_path()))),
            )
            .collect();
        steps.sort_by_key(|(line, _)| *line);
        for (_, step) in steps {
            match step {
                Step::Attribute(attr) => self.entries.push(Entry {
                    file: doc.path.as_path(),
                    line: attr.line,
                    name: attr.name.as_str(),
                    value: registry.substitute_with_docdir(&doc.path, &attr.value, &self.docdir),
                }),
                Step::Include(target) => self.visit(target, documents, graph, registry),
            }
        }
    }

    /// The `pdf-themesdir` Asciidoctor PDF uses: the last entry, else the
    /// value from the attribute registry (`.asciidoctorconfig`).
    fn themesdir(&self, registry: &AttributeRegistry) -> Option<PdfPath> {
        let value = self.last(PDF_THEMESDIR).map_or_else(
            || {
                registry
                    .resolve(self.root, PDF_THEMESDIR)
                    .map(|v| registry.substitute(self.root, v))
            },
            |entry| Some(entry.value.clone()),
        )?;
        Some(resolve_dir(&value))
    }

    fn last(&self, name: &str) -> Option<&Entry<'a>> {
        self.entries.iter().rev().find(|entry| entry.name == name)
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
    if is_unresolved(value) {
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

fn check_entry(entry: &Entry<'_>, themesdir: Option<&PdfPath>) -> Vec<DiagnosticKind> {
    if entry.name == PDF_THEME && has_extension(&entry.value, ".yaml") {
        return vec![DiagnosticKind::PdfThemeYamlExtension {
            value: entry.value.clone(),
        }];
    }
    resolve_entry(entry.name, &entry.value, themesdir)
        .into_iter()
        .filter_map(|path| match path {
            PdfPath::Absolute(path) => {
                let exists = if entry.name == PDF_THEME {
                    path.is_file()
                } else {
                    path.is_dir()
                };
                (!exists).then(|| DiagnosticKind::PdfPathNotFound {
                    attribute: entry.name.to_string(),
                    path,
                })
            }
            PdfPath::Relative(value) => Some(DiagnosticKind::UnanchoredPdfPath {
                attribute: entry.name.to_string(),
                value,
            }),
            PdfPath::Builtin(name) if entry.name == PDF_THEME && !is_builtin_theme(&name) => {
                Some(DiagnosticKind::UnknownPdfTheme { name })
            }
            PdfPath::Builtin(_) | PdfPath::Unknown(_) => None,
        })
        .collect()
}

/// Diagnostics for every `pdf-theme`, `pdf-themesdir` and `pdf-fontsdir`
/// entry, checked once per top-level document that reaches it. An entry
/// reached from several documents reports each distinct finding once, and
/// nothing when it checks clean for at least one of them.
#[must_use]
pub fn collect_pdf_diagnostics(
    documents: &HashMap<PathBuf, DocumentModel>,
    graph: &IncludeGraph,
    registry: &AttributeRegistry,
) -> Vec<Diagnostic> {
    let mut diags: Vec<Diagnostic> = Vec::new();
    let mut clean: HashSet<(PathBuf, usize)> = HashSet::new();
    for root in top_level_documents(documents, graph) {
        let scope = Scope::collect(&root, documents, graph, registry);
        let themesdir = scope.themesdir(registry);
        for entry in &scope.entries {
            let kinds = check_entry(entry, themesdir.as_ref());
            if kinds.is_empty() {
                clean.insert((entry.file.to_path_buf(), entry.line));
            }
            for kind in kinds {
                let seen = diags
                    .iter()
                    .any(|d| d.file == entry.file && d.line == entry.line && d.kind == kind);
                if !seen {
                    diags.push(Diagnostic {
                        file: entry.file.to_path_buf(),
                        line: entry.line,
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
/// else the registry value (`.asciidoctorconfig`).
#[must_use]
pub fn resolve_pdf_attribute(
    documents: &HashMap<PathBuf, DocumentModel>,
    graph: &IncludeGraph,
    registry: &AttributeRegistry,
    file: &Path,
    name: &str,
    line: Option<usize>,
) -> Option<PdfResolution> {
    if !is_pdf_path_attribute(name) {
        return None;
    }
    let roots = top_level_documents(documents, graph);
    let scope = roots
        .iter()
        .map(|root| Scope::collect(root, documents, graph, registry))
        .find(|scope| scope.files.contains(file))
        .unwrap_or_else(|| Scope::collect(file, documents, graph, registry));
    let themesdir = scope.themesdir(registry);

    let entry = match line {
        Some(line) => scope
            .entries
            .iter()
            .find(|e| e.file == file && e.line == line && e.name == name),
        None => scope.last(name),
    };
    let (value, source) = if let Some(entry) = entry {
        let source = AttributeSource {
            file: entry.file.to_path_buf(),
            line: entry.line,
        };
        (entry.value.clone(), Some(source))
    } else {
        let value = registry.resolve(scope.root, name)?;
        (
            registry.substitute(scope.root, value),
            registry.resolve_source(scope.root, name),
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
        ws.recalculate_diagnostics();
        (root, ws)
    }

    /// PDF diagnostics of `file`, as `(line, message)`, sorted by line.
    fn pdf_diags(ws: &Workspace, file: &Path) -> Vec<(usize, String)> {
        let mut diags: Vec<(usize, String)> = ws
            .diagnostics_for(file)
            .iter()
            .filter(|d| {
                matches!(
                    d.kind,
                    DiagnosticKind::PdfPathNotFound { .. }
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

    fn resolve(ws: &Workspace, file: &Path, name: &str, line: Option<usize>) -> Vec<PdfPath> {
        ws.resolve_pdf_attribute(file, name, line).unwrap().paths
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
        let missing = |p: &str| root.join(p).display().to_string();
        assert_eq!(
            pdf_diags(&ws, &root.join("missing.adoc")),
            vec![
                (
                    2,
                    format!("pdf-themesdir: not found: {}", missing("missing-themes"))
                ),
                (
                    3,
                    format!("pdf-fontsdir: not found: {}", missing("missing-fonts"))
                ),
                (
                    4,
                    format!(
                        "pdf-theme: not found: {}",
                        missing("missing-themes/custom-theme.yml")
                    )
                ),
            ]
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

    #[test]
    fn theme_without_themesdir_is_builtin_name_or_anchored_yml_path() {
        let (root, ws) = fixture();
        assert_eq!(
            pdf_diags(&ws, &root.join("no-themesdir.adoc")),
            vec![
                (
                    3,
                    "pdf-theme: 'corporate' is not a built-in theme; set pdf-themesdir for a custom theme"
                        .to_string()
                ),
                (
                    4,
                    format!(
                        "pdf-theme: theme file must end in .yml, not .yaml: {}",
                        root.join("theme/custom-theme.yaml").display()
                    )
                ),
                (
                    5,
                    "pdf-theme: relative path 'theme/custom-theme.yml' resolves against the build's working directory; anchor it with {docdir}"
                        .to_string()
                ),
                (
                    7,
                    format!(
                        "pdf-theme: not found: {}",
                        root.join("theme/none-theme.yml").display()
                    )
                ),
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
        let reference = ws
            .resolve_pdf_attribute(&main, PDF_THEMESDIR, None)
            .unwrap();
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
        ws.recalculate_diagnostics();
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
}
