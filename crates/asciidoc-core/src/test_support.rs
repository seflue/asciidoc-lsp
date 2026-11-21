//! Helpers shared by the unit tests of several modules.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::diagnostics::collect_diagnostics;
use crate::types::Diagnostic;
use crate::workspace::Workspace;

/// The diagnostics of `ws` by file, collected from its current model.
pub(crate) fn diagnostics_map(ws: &Workspace) -> HashMap<PathBuf, Vec<Diagnostic>> {
    collect_diagnostics(ws.canonical_root(), ws)
}

/// The diagnostics of one file of `ws`.
pub(crate) fn diagnostics_for(ws: &Workspace, path: &Path) -> Vec<Diagnostic> {
    diagnostics_map(ws).remove(path).unwrap_or_default()
}

/// All diagnostics of `ws` as a flat Vec.
pub(crate) fn all_diagnostics(ws: &Workspace) -> Vec<Diagnostic> {
    diagnostics_map(ws).into_values().flatten().collect()
}
