//! Fixtures shared by the unit tests of several modules.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use asciidoc_core::diagnostics::collect_diagnostics;
use asciidoc_core::types::Diagnostic;
use asciidoc_core::workspace::Workspace;

/// A scanned workspace of `files` (relative path, content), as the server
/// builds it, and the canonical path of each file.
pub fn scanned(files: &[(&str, &str)]) -> (tempfile::TempDir, Workspace, Vec<PathBuf>) {
    let tmp = tempfile::tempdir().unwrap();
    for (path, content) in files {
        let path = tmp.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
    }
    let mut ws = Workspace::new(tmp.path());
    ws.scan_workspace();
    let paths = files
        .iter()
        .map(|(path, _)| tmp.path().join(path).canonicalize().unwrap())
        .collect();
    (tmp, ws, paths)
}

/// [`scanned`], then each file opened in the editor: indexed as `did_open`
/// does, so a config file is a document of the workspace too.
pub fn opened(files: &[(&str, &str)]) -> (tempfile::TempDir, Workspace, Vec<PathBuf>) {
    let (tmp, mut ws, paths) = scanned(files);
    for path in &paths {
        if let Some(dir) = path.parent() {
            ws.set_doc_dir(path, dir);
        }
        ws.ensure_file(path, &HashMap::new());
    }
    (tmp, ws, paths)
}

/// The diagnostics of `ws` by file, collected from its current model.
pub fn diagnostics_map(ws: &Workspace) -> HashMap<PathBuf, Vec<Diagnostic>> {
    collect_diagnostics(ws.canonical_root(), ws)
}

/// The diagnostics of one file of `ws`.
pub fn diagnostics_for(ws: &Workspace, path: &Path) -> Vec<Diagnostic> {
    diagnostics_map(ws).remove(path).unwrap_or_default()
}

/// All diagnostics of `ws` as a flat Vec.
pub fn all_diagnostics(ws: &Workspace) -> Vec<Diagnostic> {
    diagnostics_map(ws).into_values().flatten().collect()
}
