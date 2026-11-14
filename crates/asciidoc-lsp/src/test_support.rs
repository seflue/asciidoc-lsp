//! Fixtures shared by the unit tests of several modules.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

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
