//! The attribute scopes a path-like value resolves in, and how features
//! present them (ADR-4). Shared by navigation and completion.

use std::path::Path;

use asciidoc_core::targets::Reading;
use asciidoc_core::workspace::{AttributeScope, Workspace};

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

/// The directory relative paths in `doc_path` resolve against: the document
/// directory from the workspace (a symlink's folder for a symlinked
/// document), else `doc_path`'s parent.
pub(crate) fn document_dir<'a>(doc_path: &'a Path, workspace: Option<&'a Workspace>) -> &'a Path {
    workspace
        .and_then(|ws| ws.attributes().doc_dir(doc_path))
        .or_else(|| doc_path.parent())
        .unwrap_or(doc_path)
}

/// How a hover names a scope: its document, marked when it is a preview.
pub(crate) fn scope_label(scope: &AttributeScope<'_>) -> String {
    label(scope.document(), scope.is_preview())
}

/// How a hover names a reading, see [`scope_label`].
pub(crate) fn reading_label(reading: &Reading) -> String {
    label(&reading.document, reading.preview)
}

fn label(document: &Path, preview: bool) -> String {
    let name = file_display_name(document);
    if preview {
        format!("{name} (preview)")
    } else {
        name
    }
}

/// Extract a display-friendly filename from a path.
pub(crate) fn file_display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |f| f.to_string_lossy().into_owned(),
    )
}
