//! Shared path-resolution helpers for `AsciiDoc` macros.
//!
//! `include::`, `xref:`, and `image::` all resolve their target path the same
//! way: substitute `{attr}` references against the source document's attribute
//! scope, then join onto the source file's directory, then canonicalize. This
//! module provides a single implementation of that pipeline so resolvers stay
//! in sync with each other (and with Asciidoctor's behaviour).

use std::path::{Path, PathBuf};

use crate::attributes::AttributeRegistry;

/// Resolve a target path string (as written in an `AsciiDoc` macro) relative
/// to the source file's directory, after applying attribute substitution.
///
/// Pipeline:
/// 1. Substitute `{attr}` references via the registry, scoped to `source`.
/// 2. Join the result onto `source.parent()`.
/// 3. Canonicalize. If the path does not exist on disk, return the joined
///    (uncanonicalized) path so callers can still compare against in-memory
///    document keys.
///
/// Returns `None` if `source` has no parent directory.
#[must_use]
pub fn resolve_doc_target(
    source: &Path,
    target: &str,
    registry: &AttributeRegistry,
) -> Option<PathBuf> {
    let resolved = registry.substitute(source, target);
    let base = source.parent()?;
    let joined = base.join(resolved);
    Some(joined.canonicalize().unwrap_or(joined))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn resolves_sibling_directory() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("a")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        let source = root.join("a/src.adoc");
        let target_file = root.join("b/doc.adoc");
        fs::write(&source, "").unwrap();
        fs::write(&target_file, "").unwrap();

        let registry = AttributeRegistry::new(&root);
        let resolved = resolve_doc_target(&source, "../b/doc.adoc", &registry).unwrap();
        assert_eq!(resolved, target_file.canonicalize().unwrap());
    }

    #[test]
    fn resolves_uncanonicalized_when_target_missing() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        // Synthetic paths (don't exist on disk) — falls back to joined path.
        let resolved = resolve_doc_target(
            Path::new("/workspace/sub/src.adoc"),
            "../other/doc.adoc",
            &registry,
        )
        .unwrap();
        assert_eq!(resolved, PathBuf::from("/workspace/sub/../other/doc.adoc"));
    }

    #[test]
    fn returns_none_when_source_has_no_parent() {
        let registry = AttributeRegistry::new(Path::new("/tmp"));
        assert!(resolve_doc_target(Path::new("/"), "doc.adoc", &registry).is_none());
    }

    #[test]
    fn substitutes_attribute_in_target() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("appendix")).unwrap();
        let source = root.join("index.adoc");
        let target_file = root.join("appendix/glossary.adoc");
        fs::write(&source, "").unwrap();
        fs::write(&target_file, "").unwrap();

        let registry = AttributeRegistry::new(&root);

        let resolved =
            resolve_doc_target(&source, "{docdir}/appendix/glossary.adoc", &registry).unwrap();
        assert_eq!(resolved, target_file.canonicalize().unwrap());
    }
}
