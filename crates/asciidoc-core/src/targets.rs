//! Where a target in a file several documents read points, one result per
//! reading (ADR-008, ADR-009, ADR-010).
//!
//! A reading is one way the file is read: by a top-level document that
//! includes it, or, for a file others include, by the editor previewing it
//! on its own. The path rule of each kind of target, the choice of readings
//! and the handling of readers that read the file only in part live here and
//! nowhere else; features ask [`CatalogCache::resolve_target`] and present.
//! Whether results that agree are shown once is the feature's decision.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::anchor_scope::CatalogCache;
use crate::attributes::DOCDIR;
use crate::paths::{
    canonicalize_if_exists, display_path, is_uri, join_doc_path, normalize_lexically,
};
pub use crate::types::{imagesdir_note, ImagesdirOrigin};
use crate::workspace::AttributeScope;

/// A target as written in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target<'t> {
    /// The path of `image::path[]` or `image:path[]`.
    Image(&'t str),
    /// The path of `include::path[]`.
    Include(&'t str),
}

/// Which readings of a file a target resolves in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readings {
    /// The top-level documents that read the file: what goto, find-references
    /// and rename follow. A target only the preview finds is broken where
    /// the file is converted (ADR-010).
    Build,
    /// Those and, for a file others include, the editor's preview of it:
    /// what hover, diagnostics and the explain report show (ADR-008).
    WithPreview,
}

/// One reading of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    /// The top-level document, or the previewed file.
    pub document: PathBuf,
    pub preview: bool,
}

/// What a target points to in one reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub reading: Reading,
    pub result: TargetResult,
}

/// The result of a [`Target`], by kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetResult {
    Image(ImageResult),
    Include(IncludeResult),
}

/// Where an include points in one reading, resolved from the folder of the
/// including file with the attributes of that reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludeResult {
    /// The path as written, attribute references substituted.
    pub substituted: String,
    /// Joined onto the folder of the including file, symlinks kept.
    pub joined: PathBuf,
    /// `joined`, canonical when it exists.
    pub path: PathBuf,
    /// A file exists at `path`. Never true for a URI.
    pub exists: bool,
}

impl IncludeResult {
    /// The target is a URI: Asciidoctor fetches it or not, it is never a
    /// file here.
    #[must_use]
    pub fn is_remote(&self) -> bool {
        is_uri(&self.substituted)
    }
}

/// Where an image points in one reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageResult {
    pub outcome: ImageOutcome,
    /// The `imagesdir` entry in effect in that reading; `None` when it is not
    /// set. Format it with [`imagesdir_note`].
    pub imagesdir_from: Option<ImagesdirOrigin>,
}

/// What an image resolves to. `path` is where the file is or would be,
/// `display` the path as hover shows it: below `imagesdir`, relative to the
/// document folder where that is short.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageOutcome {
    /// The file exists.
    Found { path: PathBuf, display: String },
    /// No file at `path`.
    Missing { path: PathBuf, display: String },
    /// A URI, in the macro target or in `imagesdir`: Asciidoctor never reads
    /// it as a file.
    Remote { url: String },
    /// The target still holds an attribute reference in this reading, which
    /// the build may set: no file can be named, so none is missing.
    Unjudged { path: PathBuf, display: String },
}

/// Where a path of a target starts from in one reading, and the typed
/// prefix as that reading substitutes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseDir {
    pub dir: PathBuf,
    pub prefix: String,
}

/// The scopes of one position and kind of reading, see
/// [`CatalogCache::resolve_target`].
pub(crate) type ReadingScopeCache<'ws> =
    HashMap<(PathBuf, usize, usize, bool), Rc<Vec<AttributeScope<'ws>>>>;

impl<'ws> CatalogCache<'ws> {
    /// Where `target` on `line` of `file` points, one result per reading in
    /// the order of [`Workspace::scopes_at`](crate::workspace::Workspace::scopes_at),
    /// the preview last.
    ///
    /// A top-level document that reads `file` only in part is no judge of
    /// what is missing in it: its failure is dropped, its hit kept.
    pub fn resolve_target(
        &self,
        file: &Path,
        line: usize,
        target: &Target<'_>,
        readings: Readings,
    ) -> Vec<Resolved> {
        let workspace = self.workspace();
        let file = workspace.indexed_path(file);
        let file_dir = workspace.attributes.doc_dir(&file).unwrap_or(&file);
        let include_dir = include_dir_of(workspace, &file);
        let builds = self.reading_scopes(&file, line, false);
        let previews = if readings == Readings::WithPreview {
            self.reading_scopes(&file, line, true)
        } else {
            Rc::default()
        };
        builds
            .iter()
            .chain(previews.iter())
            .filter_map(|scope| {
                let result = match *target {
                    Target::Image(written) => {
                        TargetResult::Image(resolve_image(scope, file_dir, written))
                    }
                    Target::Include(written) => TargetResult::Include(resolve_include(
                        &scope.substitute(written),
                        &include_dir,
                    )),
                };
                let judge = scope.is_preview() || self.get(scope.document()).reads_whole(&file);
                (judge || !result.is_failure()).then(|| Resolved {
                    reading: Reading {
                        document: scope.document().to_path_buf(),
                        preview: scope.is_preview(),
                    },
                    result,
                })
            })
            .collect()
    }

    /// Where a path of the kind of `target` starts from in each top-level
    /// document that reads `file` on `line`, with the typed text of `target`
    /// as that document substitutes it. The preview takes no part: paths are
    /// offered only where goto follows them (ADR-010). A reader of only part
    /// of the file is included: nothing is judged here.
    pub fn base_dirs(&self, file: &Path, line: usize, target: &Target<'_>) -> Vec<BaseDir> {
        let workspace = self.workspace();
        let file = workspace.indexed_path(file);
        let file_dir = workspace.attributes.doc_dir(&file).unwrap_or(&file);
        self.reading_scopes(&file, line, false)
            .iter()
            .map(|scope| match *target {
                Target::Image(typed) => BaseDir {
                    dir: image_base_dir(&docdir_of(scope, file_dir), &imagesdir_of(scope)),
                    prefix: scope.substitute(typed),
                },
                Target::Include(typed) => BaseDir {
                    dir: include_dir_of(workspace, &file),
                    prefix: scope.substitute(typed),
                },
            })
            .collect()
    }

    /// The scopes on `line` of `file`: those of the top-level documents, or
    /// with `preview` the preview alone. The scope on a line only changes
    /// with the attribute entries and includes before it, so a file's lines
    /// between the same two of them share one walk.
    fn reading_scopes(
        &self,
        file: &Path,
        line: usize,
        preview: bool,
    ) -> Rc<Vec<AttributeScope<'ws>>> {
        let workspace = self.workspace();
        let compute = || {
            if preview {
                workspace.preview_scope_at(file, line).into_iter().collect()
            } else {
                workspace.scopes_at(file, line)
            }
        };
        let Some(doc) = workspace.documents.get(file) else {
            return Rc::new(compute());
        };
        let key = (
            file.to_path_buf(),
            doc.attributes
                .iter()
                .filter(|attr| attr.line < line)
                .count(),
            doc.includes.iter().filter(|inc| inc.line < line).count(),
            preview,
        );
        if let Some(scopes) = self.reading_scopes.borrow().get(&key) {
            return Rc::clone(scopes);
        }
        let scopes = Rc::new(compute());
        self.reading_scopes
            .borrow_mut()
            .insert(key, Rc::clone(&scopes));
        scopes
    }
}

impl TargetResult {
    /// The result of a [`Target::Image`].
    #[must_use]
    pub fn into_image(self) -> Option<ImageResult> {
        match self {
            Self::Image(image) => Some(image),
            Self::Include(_) => None,
        }
    }

    /// The result of a [`Target::Include`].
    #[must_use]
    pub fn into_include(self) -> Option<IncludeResult> {
        match self {
            Self::Include(include) => Some(include),
            Self::Image(_) => None,
        }
    }

    /// The target is missing: the one result a reader of only part of the
    /// file may not give.
    fn is_failure(&self) -> bool {
        match self {
            Self::Image(image) => matches!(image.outcome, ImageOutcome::Missing { .. }),
            Self::Include(include) => !include.exists && !include.is_remote(),
        }
    }
}

/// An image target resolved without a workspace, below `folder`, with no
/// `imagesdir`.
#[must_use]
pub fn resolve_image_in_folder(written: &str, folder: &Path) -> ImageResult {
    image_result(written, folder, "", None)
}

/// An include target resolved without a workspace, from `folder`.
#[must_use]
pub fn resolve_include_in_folder(written: &str, folder: &Path) -> IncludeResult {
    resolve_include(written, folder)
}

/// The folder includes in `file` resolve from: its document folder (a
/// symlink's own folder for a symlinked file), else its parent.
fn include_dir_of(workspace: &crate::workspace::Workspace, file: &Path) -> PathBuf {
    workspace
        .attributes
        .doc_dir(file)
        .or_else(|| file.parent())
        .unwrap_or(file)
        .to_path_buf()
}

/// The include `substituted` from the folder `dir`.
fn resolve_include(substituted: &str, dir: &Path) -> IncludeResult {
    let joined = join_doc_path(dir, substituted);
    let path = canonicalize_if_exists(joined.clone());
    IncludeResult {
        substituted: substituted.to_string(),
        exists: !is_uri(substituted) && path.exists(),
        joined,
        path,
    }
}

/// The folder paths resolve against in `scope`: its `docdir`, else `file_dir`.
fn docdir_of(scope: &AttributeScope<'_>, file_dir: &Path) -> PathBuf {
    scope
        .value(DOCDIR)
        .map_or_else(|| file_dir.to_path_buf(), PathBuf::from)
}

fn imagesdir_of(scope: &AttributeScope<'_>) -> String {
    scope.value("imagesdir").unwrap_or_default()
}

/// `docdir / imagesdir`, or `docdir` alone when `imagesdir` is empty.
fn image_base_dir(docdir: &Path, imagesdir: &str) -> PathBuf {
    if imagesdir.is_empty() {
        docdir.to_path_buf()
    } else {
        normalize_lexically(&docdir.join(imagesdir))
    }
}

/// The image `written` in `scope`: against `imagesdir` below `docdir`, as
/// Asciidoctor resolves it.
fn resolve_image(scope: &AttributeScope<'_>, file_dir: &Path, written: &str) -> ImageResult {
    let imagesdir = imagesdir_of(scope);
    let from = Some(&imagesdir)
        .filter(|value| !value.is_empty())
        .and_then(|_| scope.definition("imagesdir")?.source)
        .map(|source| ImagesdirOrigin {
            file: source.file,
            line: source.line,
        });
    image_result(
        &scope.substitute(written),
        &docdir_of(scope, file_dir),
        &imagesdir,
        from,
    )
}

fn image_result(
    target: &str,
    docdir: &Path,
    imagesdir: &str,
    imagesdir_from: Option<ImagesdirOrigin>,
) -> ImageResult {
    let mut display = if imagesdir.is_empty() || is_uri(target) {
        target.to_string()
    } else {
        format!("{}/{target}", imagesdir.trim_end_matches('/'))
    };
    if Path::new(&display).is_absolute() {
        display = display_path(docdir, Path::new(&display))
            .display()
            .to_string();
    }
    let outcome = if is_uri(&display) {
        ImageOutcome::Remote { url: display }
    } else {
        let path = join_doc_path(&image_base_dir(docdir, imagesdir), target);
        if target.contains('{') {
            ImageOutcome::Unjudged { path, display }
        } else if path.exists() {
            ImageOutcome::Found { path, display }
        } else {
            ImageOutcome::Missing { path, display }
        }
    };
    ImageResult {
        outcome,
        imagesdir_from,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;
    use pretty_assertions::assert_eq;

    fn fixture(name: &str) -> (PathBuf, Workspace) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(name)
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        (root, ws)
    }

    /// A workspace of `files` (relative path, content) in a temporary folder.
    fn scanned(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, Workspace) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for (path, content) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        (tmp, root, ws)
    }

    /// One image's results as (document relative to `root`, preview,
    /// outcome with paths relative to `root`).
    fn image(
        root: &Path,
        ws: &Workspace,
        file: &str,
        line: usize,
        written: &str,
        readings: Readings,
    ) -> Vec<(String, bool, String)> {
        let relative = |path: &Path| path.strip_prefix(root).unwrap().display().to_string();
        CatalogCache::new(ws)
            .resolve_target(&root.join(file), line, &Target::Image(written), readings)
            .into_iter()
            .map(|resolved| {
                let result = resolved.result.into_image().unwrap();
                let outcome = match result.outcome {
                    ImageOutcome::Found { path, display } => {
                        format!("found {} as {display}", relative(&path))
                    }
                    ImageOutcome::Missing { path, display } => {
                        format!("missing {} as {display}", relative(&path))
                    }
                    ImageOutcome::Remote { url } => format!("remote {url}"),
                    ImageOutcome::Unjudged { display, .. } => format!("unjudged {display}"),
                };
                (
                    relative(&resolved.reading.document),
                    resolved.reading.preview,
                    outcome,
                )
            })
            .collect()
    }

    fn origin_of(
        root: &Path,
        ws: &Workspace,
        file: &str,
        line: usize,
        written: &str,
    ) -> Vec<Option<(String, usize)>> {
        CatalogCache::new(ws)
            .resolve_target(
                &root.join(file),
                line,
                &Target::Image(written),
                Readings::WithPreview,
            )
            .into_iter()
            .map(|resolved| {
                let result = resolved.result.into_image().unwrap();
                result.imagesdir_from.map(|origin| {
                    (
                        origin
                            .file
                            .strip_prefix(root)
                            .unwrap()
                            .display()
                            .to_string(),
                        origin.line,
                    )
                })
            })
            .collect()
    }

    fn reading(document: &str, preview: bool, outcome: &str) -> (String, bool, String) {
        (document.to_string(), preview, outcome.to_string())
    }

    #[test]
    fn test_image_resolves_below_docdir_and_imagesdir_of_the_reading() {
        let (root, ws) = fixture("paths_in_include_preview");

        // The master has `imagesdir: images`, the preview reads the chapter
        // alone: no `imagesdir`, its own folder as `docdir`.
        assert_eq!(
            image(
                &root,
                &ws,
                "chapters/chapter.adoc",
                8,
                "found.png",
                Readings::WithPreview
            ),
            vec![
                reading(
                    "index.adoc",
                    false,
                    "found images/found.png as images/found.png"
                ),
                reading(
                    "chapters/chapter.adoc",
                    true,
                    "missing chapters/found.png as found.png"
                ),
            ]
        );
        assert_eq!(
            image(
                &root,
                &ws,
                "chapters/chapter.adoc",
                10,
                "preview.png",
                Readings::WithPreview
            ),
            vec![
                reading(
                    "index.adoc",
                    false,
                    "missing images/preview.png as images/preview.png"
                ),
                reading(
                    "chapters/chapter.adoc",
                    true,
                    "found chapters/preview.png as preview.png"
                ),
            ]
        );
    }

    #[test]
    fn test_build_readings_leave_out_the_preview() {
        let (root, ws) = fixture("paths_in_include_preview");

        assert_eq!(
            image(
                &root,
                &ws,
                "chapters/chapter.adoc",
                10,
                "preview.png",
                Readings::Build
            ),
            vec![reading(
                "index.adoc",
                false,
                "missing images/preview.png as images/preview.png"
            )]
        );
    }

    #[test]
    fn test_file_no_document_includes_is_its_own_reading_without_preview() {
        let (root, ws) = fixture("image_scopes");

        assert_eq!(
            image(
                &root,
                &ws,
                "solo.adoc",
                3,
                "missing.png",
                Readings::WithPreview
            ),
            vec![reading(
                "solo.adoc",
                false,
                "missing missing.png as missing.png"
            )]
        );
    }

    #[test]
    fn test_one_result_per_top_level_document_in_scope_order_preview_last() {
        let (root, ws) = fixture("image_scopes");

        let readings: Vec<(String, bool)> = image(
            &root,
            &ws,
            "shared.adoc",
            3,
            "logo.png",
            Readings::WithPreview,
        )
        .into_iter()
        .map(|(document, preview, _)| (document, preview))
        .collect();
        assert_eq!(
            readings,
            vec![
                ("book.adoc".to_string(), false),
                ("web.adoc".to_string(), false),
                ("shared.adoc".to_string(), true),
            ]
        );
    }

    #[test]
    fn test_image_through_symlinked_assets_folder_keeps_the_link() {
        let (root, ws) = fixture("symlinks");

        assert_eq!(
            image(
                &root,
                &ws,
                "real/project/docs/ch3.adoc",
                4,
                "linked.svg",
                Readings::Build
            ),
            vec![reading(
                "real/project/docs/ch3.adoc",
                false,
                "found real/project/shared-assets/linked.svg as ../shared-assets/linked.svg"
            )]
        );
    }

    #[test]
    fn test_image_attribute_in_target_is_substituted_in_each_reading() {
        let (root, ws) = fixture("symlinks");

        assert_eq!(
            image(
                &root,
                &ws,
                "real/project/docs/ch1.adoc",
                10,
                "{logo-name}",
                Readings::Build
            ),
            vec![reading(
                "real/project/docs/ch1.adoc",
                false,
                "found real/project/assets/logo.svg as ../assets/logo.svg"
            )]
        );
    }

    #[test]
    fn test_partial_reader_miss_is_dropped_and_its_hit_kept() {
        let part = "// tag::a[]\nText.\n// end::a[]\n\nimage::logo.png[]\n";
        // `web.adoc` reads `part.adoc` in part, so it is no judge of what
        // is missing there.
        let (_tmp, root, ws) = scanned(&[
            (
                "book.adoc",
                "= Book\n:imagesdir: img\n\ninclude::part.adoc[]\n",
            ),
            (
                "web.adoc",
                "= Web\n:imagesdir: nowhere\n\ninclude::part.adoc[tag=a]\n",
            ),
            ("part.adoc", part),
            ("img/logo.png", "x"),
            ("logo.png", "x"),
        ]);
        assert_eq!(
            image(
                &root,
                &ws,
                "part.adoc",
                5,
                "logo.png",
                Readings::WithPreview
            ),
            vec![
                reading("book.adoc", false, "found img/logo.png as img/logo.png"),
                reading("part.adoc", true, "found logo.png as logo.png"),
            ]
        );

        // The partial reader finds it, the whole reader does not: both stay.
        let (_tmp, root, ws) = scanned(&[
            (
                "book.adoc",
                "= Book\n:imagesdir: nowhere\n\ninclude::part.adoc[]\n",
            ),
            (
                "web.adoc",
                "= Web\n:imagesdir: img\n\ninclude::part.adoc[tag=a]\n",
            ),
            ("part.adoc", part),
            ("img/logo.png", "x"),
        ]);
        assert_eq!(
            image(&root, &ws, "part.adoc", 5, "logo.png", Readings::Build),
            vec![
                reading(
                    "book.adoc",
                    false,
                    "missing nowhere/logo.png as nowhere/logo.png"
                ),
                reading("web.adoc", false, "found img/logo.png as img/logo.png"),
            ]
        );
    }

    #[rstest::rstest]
    #[case::uri_target(
        "= T\n\nimage::https://example.com/x.png[]\n",
        "https://example.com/x.png",
        "https://example.com/x.png"
    )]
    #[case::data_uri_target(
        "= T\n\nimage::data:image/png;base64,AAAA[]\n",
        "data:image/png;base64,AAAA",
        "data:image/png;base64,AAAA"
    )]
    #[case::uri_imagesdir(
        "= T\n:imagesdir: https://cdn.example.com/img\n\nimage::x.png[]\n",
        "x.png",
        "https://cdn.example.com/img/x.png"
    )]
    fn test_image_with_uri_is_remote(#[case] text: &str, #[case] written: &str, #[case] url: &str) {
        let (_tmp, root, ws) = scanned(&[("main.adoc", text)]);
        let line = text.lines().count();

        assert_eq!(
            image(
                &root,
                &ws,
                "main.adoc",
                line,
                written,
                Readings::WithPreview
            ),
            vec![reading("main.adoc", false, &format!("remote {url}"))]
        );
    }

    #[test]
    fn test_image_with_unresolved_reference_is_not_judged() {
        let (_tmp, root, ws) = scanned(&[("main.adoc", "= T\n\nimage::{unset-dir}/x.png[]\n")]);

        assert_eq!(
            image(
                &root,
                &ws,
                "main.adoc",
                3,
                "{unset-dir}/x.png",
                Readings::WithPreview
            ),
            vec![reading("main.adoc", false, "unjudged {unset-dir}/x.png")]
        );
    }

    #[test]
    fn test_imagesdir_origin_follows_the_include_before_the_image() {
        let (root, ws) = fixture("image_scopes_includes");

        assert_eq!(
            origin_of(&root, &ws, "reverse.adoc", 3, "top.png"),
            vec![None]
        );
        assert_eq!(
            origin_of(&root, &ws, "reverse.adoc", 7, "top.png"),
            vec![Some(("setdir.adoc".to_string(), 1))]
        );
    }

    #[test]
    fn test_imagesdir_origin_is_none_when_imagesdir_is_empty() {
        let (_tmp, root, ws) = scanned(&[("main.adoc", "= T\n:imagesdir:\n\nimage::x.png[]\n")]);

        assert_eq!(origin_of(&root, &ws, "main.adoc", 4, "x.png"), vec![None]);
    }

    #[test]
    fn test_imagesdir_note_names_the_file_as_the_caller_shows_it() {
        let origin = ImagesdirOrigin {
            file: PathBuf::from("/project/chapters/part.adoc"),
            line: 3,
        };

        assert_eq!(
            imagesdir_note(Some(&origin), |file| file
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()),
            "imagesdir from part.adoc:3"
        );
        assert_eq!(
            imagesdir_note(Some(&origin), |file| file
                .strip_prefix("/project")
                .unwrap()
                .display()
                .to_string()),
            "imagesdir from chapters/part.adoc:3"
        );
        assert_eq!(
            imagesdir_note(None, |_| unreachable!("no file to name")),
            "imagesdir not set in this build"
        );
    }

    #[test]
    fn test_image_without_workspace_resolves_below_the_folder() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("here.png"), "x").unwrap();

        let found = resolve_image_in_folder("here.png", tmp.path());
        assert_eq!(
            found.outcome,
            ImageOutcome::Found {
                path: tmp.path().join("here.png"),
                display: "here.png".to_string()
            }
        );
        assert_eq!(found.imagesdir_from, None);
        assert!(matches!(
            resolve_image_in_folder("gone.png", tmp.path()).outcome,
            ImageOutcome::Missing { .. }
        ));
    }

    #[test]
    fn test_base_dirs_start_from_each_build_reading_without_the_preview() {
        let (root, ws) = fixture("paths_in_include_preview");
        let bases = CatalogCache::new(&ws).base_dirs(
            &root.join("chapters/chapter.adoc"),
            8,
            &Target::Image("{docdir}/fo"),
        );

        assert_eq!(
            bases,
            vec![BaseDir {
                dir: root.join("images"),
                prefix: format!("{}/fo", root.display()),
            }]
        );
    }

    #[test]
    fn test_base_dirs_include_a_reader_of_only_part_of_the_file() {
        let part = "// tag::a[]\nText.\n// end::a[]\n";
        let (_tmp, root, ws) = scanned(&[
            (
                "book.adoc",
                "= Book\n:imagesdir: img\n\ninclude::part.adoc[]\n",
            ),
            (
                "web.adoc",
                "= Web\n:imagesdir: pics\n\ninclude::part.adoc[tag=a]\n",
            ),
            ("part.adoc", part),
        ]);
        let dirs: Vec<PathBuf> = CatalogCache::new(&ws)
            .base_dirs(&root.join("part.adoc"), 2, &Target::Image("x"))
            .into_iter()
            .map(|base| base.dir)
            .collect();

        assert_eq!(dirs, vec![root.join("img"), root.join("pics")]);
    }

    /// One include's results as (document relative to `root`, preview,
    /// outcome with paths relative to `root`).
    fn include(
        root: &Path,
        ws: &Workspace,
        file: &str,
        line: usize,
        written: &str,
        readings: Readings,
    ) -> Vec<(String, bool, String)> {
        let relative = |path: &Path| path.strip_prefix(root).unwrap().display().to_string();
        CatalogCache::new(ws)
            .resolve_target(&root.join(file), line, &Target::Include(written), readings)
            .into_iter()
            .map(|resolved| {
                let result = resolved.result.into_include().unwrap();
                let outcome = if result.is_remote() {
                    format!("remote {}", result.substituted)
                } else if result.exists {
                    format!("found {}", relative(&result.path))
                } else {
                    format!("missing {}", relative(&result.joined))
                };
                (
                    relative(&resolved.reading.document),
                    resolved.reading.preview,
                    outcome,
                )
            })
            .collect()
    }

    #[test]
    fn test_include_resolves_from_the_including_folder_with_the_attributes_of_the_reading() {
        let (root, ws) = fixture("paths_in_include_preview");

        // `{docdir}` is the folder of the master in the build reading, the
        // folder of the chapter in the preview, where `shared/` is missing.
        assert_eq!(
            include(
                &root,
                &ws,
                "chapters/chapter.adoc",
                14,
                "{docdir}/shared/found.adoc",
                Readings::WithPreview
            ),
            vec![
                reading("index.adoc", false, "found shared/found.adoc"),
                reading(
                    "chapters/chapter.adoc",
                    true,
                    "missing chapters/shared/found.adoc"
                ),
            ]
        );
        // The path as written is joined onto the including folder, not the
        // docdir of the reading.
        assert_eq!(
            include(
                &root,
                &ws,
                "chapters/chapter.adoc",
                16,
                "parts/preview.adoc",
                Readings::Build
            ),
            vec![reading(
                "index.adoc",
                false,
                "found chapters/parts/preview.adoc"
            )]
        );
    }

    #[test]
    fn test_include_build_leaves_out_the_preview_and_with_preview_adds_it_last() {
        let (root, ws) = fixture("paths_in_include_preview");

        // Only the preview finds the file the docdir of the master lacks.
        assert_eq!(
            include(
                &root,
                &ws,
                "chapters/chapter.adoc",
                16,
                "{docdir}/parts/preview.adoc",
                Readings::Build
            ),
            vec![reading("index.adoc", false, "missing parts/preview.adoc")]
        );
        assert_eq!(
            include(
                &root,
                &ws,
                "chapters/chapter.adoc",
                16,
                "{docdir}/parts/preview.adoc",
                Readings::WithPreview
            ),
            vec![
                reading("index.adoc", false, "missing parts/preview.adoc"),
                reading(
                    "chapters/chapter.adoc",
                    true,
                    "found chapters/parts/preview.adoc"
                ),
            ]
        );
    }

    #[test]
    fn test_include_of_a_reader_of_only_part_of_the_file_drops_its_failure_keeps_its_hit() {
        let part = "// tag::a[]\ninclude::{inc}[]\n// end::a[]\n";
        let (_tmp, root, ws) = scanned(&[
            (
                "book.adoc",
                "= Book\n:inc: a.adoc\n\ninclude::part.adoc[]\n",
            ),
            (
                "web.adoc",
                "= Web\n:inc: b.adoc\n\ninclude::part.adoc[tag=a]\n",
            ),
            ("part.adoc", part),
            ("a.adoc", "A\n"),
        ]);

        // `b.adoc` is missing in the partial reader: dropped, not judged.
        assert_eq!(
            include(&root, &ws, "part.adoc", 2, "{inc}", Readings::Build),
            vec![reading("book.adoc", false, "found a.adoc")]
        );

        // Where the partial reader hits, it is kept.
        let (_tmp, root, ws) = scanned(&[
            (
                "book.adoc",
                "= Book\n:inc: a.adoc\n\ninclude::part.adoc[]\n",
            ),
            (
                "web.adoc",
                "= Web\n:inc: a.adoc\n\ninclude::part.adoc[tag=a]\n",
            ),
            ("part.adoc", part),
            ("a.adoc", "A\n"),
        ]);
        assert_eq!(
            include(&root, &ws, "part.adoc", 2, "{inc}", Readings::Build),
            vec![
                reading("book.adoc", false, "found a.adoc"),
                reading("web.adoc", false, "found a.adoc"),
            ]
        );
    }

    #[test]
    fn test_include_with_a_uri_target_is_remote_never_missing() {
        let (_tmp, root, ws) = scanned(&[(
            "book.adoc",
            "= Book\n:remote: https://example.com/a.adoc\n\ninclude::{remote}[]\n\ninclude::https://example.com/b.adoc[]\n",
        )]);

        assert_eq!(
            include(&root, &ws, "book.adoc", 4, "{remote}", Readings::Build),
            vec![reading(
                "book.adoc",
                false,
                "remote https://example.com/a.adoc"
            )]
        );
        let results = CatalogCache::new(&ws).resolve_target(
            &root.join("book.adoc"),
            6,
            &Target::Include("https://example.com/b.adoc"),
            Readings::Build,
        );
        let [only] = results.as_slice() else {
            panic!("one reading expected");
        };
        assert!(!only.result.is_failure());
    }

    #[test]
    fn test_include_missing_in_one_build_reading_is_reported_for_that_reading_only() {
        let (_tmp, root, ws) = scanned(&[
            ("book.adoc", "= Book\n:dir: a\n\ninclude::part.adoc[]\n"),
            ("web.adoc", "= Web\n:dir: b\n\ninclude::part.adoc[]\n"),
            ("part.adoc", "include::{dir}.adoc[]\n"),
            ("a.adoc", "A\n"),
        ]);

        assert_eq!(
            include(&root, &ws, "part.adoc", 1, "{dir}.adoc", Readings::Build),
            vec![
                reading("book.adoc", false, "found a.adoc"),
                reading("web.adoc", false, "missing b.adoc"),
            ]
        );
    }

    #[test]
    fn test_include_in_a_file_reached_through_a_folder_link_resolves_from_the_link_folder() {
        let (root, _) = fixture("symlinks");
        let project = root.join("real/project");
        let ws = Workspace::build(&project, &project.join("diamond.adoc"));

        // `up.adoc` is first met as `guide/up.adoc`, so `../part-up.adoc` is
        // the one next to `guide`, not next to the file's real folder.
        assert_eq!(
            include(
                &root,
                &ws,
                "real/outside/guide/up.adoc",
                3,
                "../part-up.adoc",
                Readings::Build
            ),
            vec![reading(
                "real/project/diamond.adoc",
                false,
                "found real/project/part-up.adoc"
            )]
        );
    }

    #[test]
    fn test_include_found_through_a_folder_link_has_a_canonical_path_and_a_link_path() {
        let (root, ws) = fixture("symlinks");

        let results = CatalogCache::new(&ws).resolve_target(
            &root.join("real/project/via-link.adoc"),
            3,
            &Target::Include("config/book-link/twice.adoc"),
            Readings::Build,
        );
        let include = results[0].result.clone().into_include().unwrap();
        assert_eq!(
            include.joined,
            root.join("real/project/config/book-link/twice.adoc")
        );
        assert_eq!(include.path, root.join("real/project/book/twice.adoc"));
        assert!(include.exists);
    }

    #[test]
    fn test_include_base_dirs_start_from_the_including_folder_with_the_reading_prefix() {
        let (root, ws) = fixture("paths_in_include_preview");
        let bases = CatalogCache::new(&ws).base_dirs(
            &root.join("chapters/chapter.adoc"),
            14,
            &Target::Include("{docdir}/sh"),
        );

        assert_eq!(
            bases,
            vec![BaseDir {
                dir: root.join("chapters"),
                prefix: format!("{}/sh", root.display()),
            }]
        );
    }

    #[test]
    fn test_include_in_folder_resolves_without_a_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.adoc"), "A\n").unwrap();

        let found = resolve_include_in_folder("a.adoc", &root);
        assert!(found.exists);
        assert_eq!(found.path, root.join("a.adoc"));
        let missing = resolve_include_in_folder("b.adoc", &root);
        assert!(!missing.exists);
        assert!(resolve_include_in_folder("https://example.com/x.adoc", &root).is_remote());
    }
}
