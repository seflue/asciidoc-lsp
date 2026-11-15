//! The attribute scopes a path-like value resolves in, and how features
//! present them (ADR-4). Shared by navigation and completion.

use std::path::{Path, PathBuf};

use asciidoc_core::attributes::DOCDIR;
use asciidoc_core::paths::{canonicalize_if_exists, join_doc_path, normalize_lexically};
use asciidoc_core::workspace::{AttributeScope, Workspace};

/// The scopes a path on `line` of `doc_path` resolves in: one per
/// top-level document and, for a file others include, the editor's preview
/// of it (ADR-4).
pub(crate) fn path_scopes<'w>(
    ws: &'w Workspace,
    doc_path: &Path,
    line: usize,
) -> impl Iterator<Item = AttributeScope<'w>> {
    ws.scopes_at(doc_path, line)
        .into_iter()
        .chain(ws.preview_scope_at(doc_path, line))
}

/// Keeps only the first of `items` when every neighbouring pair is `same`:
/// scopes that agree give one result, not one per scope (ADR-4).
pub(crate) fn collapse_if_agree<T>(items: &mut Vec<T>, same: impl Fn(&T, &T) -> bool) {
    if items.windows(2).all(|pair| same(&pair[0], &pair[1])) {
        items.truncate(1);
    }
}

/// `items` without repeats, each kept at its first position.
pub(crate) fn unique_in_order<T: PartialEq>(items: impl IntoIterator<Item = T>) -> Vec<T> {
    let mut unique = Vec::new();
    for item in items {
        if !unique.contains(&item) {
            unique.push(item);
        }
    }
    unique
}

/// The folder image paths resolve against in `scope` and the `imagesdir`
/// shown in front of them: `docdir` (else `file_dir`) joined with
/// `imagesdir`.
pub(crate) fn scope_image_base(scope: &AttributeScope<'_>, file_dir: &Path) -> (PathBuf, String) {
    let imagesdir = scope.value("imagesdir").unwrap_or_default();
    let docdir = scope.value(DOCDIR).map(PathBuf::from);
    let base = image_base_dir(docdir.as_deref().unwrap_or(file_dir), &imagesdir);
    (base, imagesdir)
}

/// The directory relative paths in `doc_path` resolve against: the document
/// directory from the workspace (a symlink's folder for a symlinked
/// document), else `doc_path`'s parent.
pub(crate) fn document_dir<'a>(doc_path: &'a Path, workspace: Option<&'a Workspace>) -> &'a Path {
    workspace
        .and_then(|ws| ws.attributes().doc_dir(doc_path))
        .or_else(|| doc_path.parent())
        .unwrap_or(doc_path)
}

/// `doc_dir / imagesdir`, or `doc_dir` alone when `imagesdir` is empty.
fn image_base_dir(doc_dir: &Path, imagesdir: &str) -> PathBuf {
    if imagesdir.is_empty() {
        doc_dir.to_path_buf()
    } else {
        normalize_lexically(&doc_dir.join(imagesdir))
    }
}

/// How a hover names a scope: its document, marked when it is a preview.
pub(crate) fn scope_label(scope: &AttributeScope<'_>) -> String {
    let name = file_display_name(scope.document());
    if scope.is_preview() {
        format!("{name} (preview)")
    } else {
        name
    }
}

/// One scope's view of an include or xref file path.
pub(crate) struct DocTarget {
    pub(crate) label: String,
    /// The path as written, attribute references substituted.
    pub(crate) substituted: String,
    /// Joined onto the document directory, symlinks kept.
    pub(crate) joined: PathBuf,
    /// `joined`, canonical when it exists.
    pub(crate) path: PathBuf,
}

impl DocTarget {
    pub(crate) fn new(label: String, substituted: &str, dir: &Path) -> Self {
        let joined = join_doc_path(dir, substituted);
        Self {
            label,
            substituted: substituted.to_string(),
            path: canonicalize_if_exists(joined.clone()),
            joined,
        }
    }
}

/// Extract a display-friendly filename from a path.
pub(crate) fn file_display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |f| f.to_string_lossy().into_owned(),
    )
}
