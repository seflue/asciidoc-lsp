//! Shared path-resolution helpers for `AsciiDoc` macros.
//!
//! `include::`, `xref:`, and `image::` all resolve their target path the same
//! way: substitute `{attr}` references against the source document's attribute
//! scope, then join onto the source file's document directory, pop `..`
//! segments textually, then canonicalize. This
//! module provides a single implementation of that pipeline so resolvers stay
//! in sync with each other (and with Asciidoctor's behaviour).

use std::path::{Component, Path, PathBuf};

use crate::attributes::AttributeRegistry;

/// Resolve a target path string (as written in an `AsciiDoc` macro) relative
/// to the source file's document directory, after applying attribute substitution.
///
/// Pipeline:
/// 1. Substitute `{attr}` references via the registry, scoped to `source`.
/// 2. Join the result onto the document directory of `source`
///    ([`AttributeRegistry::doc_dir`]): the symlink's folder for a document
///    opened through a symlink, as with Asciidoctor's `docdir`.
/// 3. Pop `..` segments textually ([`normalize_lexically`]).
/// 4. Canonicalize. If the path does not exist on disk, return the normalized
///    (uncanonicalized) path so callers can still compare against in-memory
///    document keys.
///
/// Returns `None` if `source` has no document directory.
#[must_use]
pub fn resolve_doc_target(
    source: &Path,
    target: &str,
    registry: &AttributeRegistry,
) -> Option<PathBuf> {
    resolve_doc_path(source, target, registry).map(canonicalize_if_exists)
}

/// Steps 2–3 of [`resolve_doc_target`] for a target already substituted: join
/// it onto the document directory `dir` and pop `..` segments textually.
/// Symlinks are kept.
#[must_use]
pub fn join_doc_path(dir: &Path, substituted: &str) -> PathBuf {
    normalize_lexically(&dir.join(substituted))
}

/// Step 4 of [`resolve_doc_target`]: the canonical form of `path`, or `path`
/// itself when it does not exist on disk.
#[must_use]
pub fn canonicalize_if_exists(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

/// Steps 1–3 of [`resolve_doc_target`]: the target as Asciidoctor sees it,
/// with symlinks kept. Its parent is the document directory of the target.
///
/// A target with `{docdir}` in a file that several top-level documents
/// include ([`AttributeRegistry::docdirs`]) resolves to the first candidate
/// that exists, else to the first one: it is broken only if it is broken
/// for every including document.
#[must_use]
pub fn resolve_doc_path(
    source: &Path,
    target: &str,
    registry: &AttributeRegistry,
) -> Option<PathBuf> {
    let base = registry.doc_dir(source)?;
    let join = |resolved: String| join_doc_path(base, &resolved);
    let docdirs = registry.docdirs(source);
    if docdirs.len() < 2 || !target.contains("{docdir}") {
        return Some(join(registry.substitute(source, target)));
    }
    let mut candidates = docdirs
        .iter()
        .map(|docdir| join(registry.substitute_with_docdir(source, target, docdir)));
    let first = candidates.next()?;
    if first.exists() {
        return Some(first);
    }
    Some(candidates.find(|path| path.exists()).unwrap_or(first))
}

/// Drop `.` segments and let each `..` pop the segment before it, without
/// asking the filesystem. Asciidoctor's `PathResolver#system_path` resolves
/// paths this way, so `..` in a symlinked folder goes to the link's parent,
/// not the target's. A `..` at the root or at the start of a relative path
/// is kept as the root or as `..`. A relative path that pops to nothing
/// (`a/..`) gives an empty path.
#[must_use]
pub fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match normalized.components().next_back() {
                Some(Component::Normal(_)) => {
                    normalized.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => normalized.push(".."),
            },
            other => normalized.push(other),
        }
    }
    normalized
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
        // Synthetic paths (don't exist on disk) — falls back to the
        // joined path with `..` popped textually.
        let resolved = resolve_doc_target(
            Path::new("/workspace/sub/src.adoc"),
            "../other/doc.adoc",
            &registry,
        )
        .unwrap();
        assert_eq!(resolved, PathBuf::from("/workspace/other/doc.adoc"));
    }

    /// A `{docdir}` target that exists for no including document resolves
    /// with the first one's `docdir`, so it reports as broken.
    #[test]
    fn docdir_target_missing_for_every_document_uses_first_docdir() {
        let mut registry = AttributeRegistry::new(Path::new("/ws"));
        let part = Path::new("/ws/shared/part.adoc");
        let roots = std::collections::HashMap::from([(
            part.to_path_buf(),
            vec![PathBuf::from("/ws/a/a.adoc"), PathBuf::from("/ws/b/b.adoc")],
        )]);
        registry.set_doc_roots(&roots);
        assert_eq!(
            resolve_doc_target(part, "{docdir}/x.adoc", &registry),
            Some(PathBuf::from("/ws/a/x.adoc"))
        );
    }

    #[test]
    fn normalize_lexically_pops_parent_segments() {
        for (input, expected) in [
            ("/a/b/../c/./d.adoc", "/a/c/d.adoc"),
            ("/a/../../b", "/b"),
            ("../a/../b", "../b"),
            ("a/../../b", "../b"),
        ] {
            assert_eq!(
                normalize_lexically(Path::new(input)),
                PathBuf::from(expected),
                "{input}"
            );
        }
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
