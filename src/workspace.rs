use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::attributes::AttributeRegistry;
use crate::diagnostics::collect_diagnostics;
use crate::graph::{build_graph, IncludeGraph};
use crate::references::ReferenceIndex;
use crate::types::{Diagnostic, DocumentModel};

#[allow(dead_code)] // Fields are public API for LSP consumers
pub struct Workspace {
    pub root: PathBuf,
    pub attributes: AttributeRegistry,
    pub include_graph: IncludeGraph,
    pub reference_index: ReferenceIndex,
    pub documents: HashMap<PathBuf, DocumentModel>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Workspace {
    pub fn build(root: &Path, entry: &Path) -> Result<Self> {
        let mut registry = AttributeRegistry::new(root);
        registry.load_config(root);
        let (graph, documents) = build_graph(entry, &mut registry);
        let ref_index = ReferenceIndex::build(&documents, &registry);
        let diagnostics = collect_diagnostics(&documents, &graph, &ref_index, &registry);
        Ok(Self {
            root: root.into(),
            attributes: registry,
            include_graph: graph,
            reference_index: ref_index,
            documents,
            diagnostics,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_workspace_build_simple() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(&a, "= Doc A\n\ninclude::b.adoc[]\n").unwrap();
        fs::write(&b, "== Chapter B\n\nContent.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();

        assert_eq!(ws.documents.len(), 2);
        assert!(!ws.include_graph.forward.is_empty());
        // No broken refs expected
        let broken_xrefs: Vec<_> = ws
            .diagnostics
            .iter()
            .filter(|d| matches!(d.kind, crate::types::DiagnosticKind::BrokenXref { .. }))
            .collect();
        assert!(
            broken_xrefs.is_empty(),
            "Expected no broken xrefs, got: {:?}",
            broken_xrefs
        );
    }

    #[test]
    fn test_workspace_build_with_xref() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        fs::write(
            &a,
            "= Doc A\n\n[[my-anchor]]\n== Section A\n\ninclude::b.adoc[]\n",
        )
        .unwrap();
        fs::write(&b, "== Chapter B\n\nSee <<my-anchor>>.\n").unwrap();

        let ws = Workspace::build(tmp.path(), &a).unwrap();

        assert_eq!(ws.documents.len(), 2);
        assert!(ws.reference_index.anchors.contains_key("my-anchor"));
    }
}
