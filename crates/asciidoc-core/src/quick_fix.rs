//! Quick-fixes for broken xrefs and broken includes: the nearest existing ID
//! or file name replaces the typo. Only the ID of an xref and the last
//! segment of an include path are replaced, never link text, file part,
//! attributes or brackets.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::anchor_scope::{AnchorScope, CatalogCache};
use crate::references::parse_xref_target;
use crate::targets::{Readings, Target};
use crate::types::{Diagnostic, DiagnosticKind, ResolvedXref};
use crate::workspace::Workspace;

/// A text replacement on one line; columns are byte columns like `Span`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    pub line: usize,
    pub start_col: usize,
    pub end_col: usize,
    pub new_text: String,
    /// The only candidate at the smallest distance, so a client may apply it
    /// without asking.
    pub preferred: bool,
}

/// The most fixes offered for one diagnostic.
const MAX_FIXES: usize = 3;

/// The quick-fixes for `diagnostic`, which belongs to the file `path`: empty
/// for every kind but a broken xref or include, and wherever no replacement
/// is certain to resolve the diagnostic.
#[must_use]
pub fn quick_fixes(
    workspace: &Workspace,
    path: &Path,
    diagnostic: &Diagnostic,
) -> Vec<Replacement> {
    match &diagnostic.kind {
        DiagnosticKind::BrokenXref { target, .. } => {
            xref_fixes(workspace, path, diagnostic, target)
        }
        DiagnosticKind::BrokenInclude { path: written, .. }
        | DiagnosticKind::OptionalIncludeNotFound { path: written } => {
            include_fixes(workspace, path, diagnostic, written)
        }
        _ => Vec::new(),
    }
}

/// The candidates within the typo distance of `typed`, nearest first, then
/// alphabetical, each with its distance.
fn near<'a>(typed: &str, candidates: impl IntoIterator<Item = &'a str>) -> Vec<(&'a str, usize)> {
    let limit = typed.chars().count().max(3) / 3;
    let mut found: Vec<(&str, usize)> = candidates
        .into_iter()
        .filter(|candidate| *candidate != typed)
        .map(|candidate| (candidate, strsim::osa_distance(typed, candidate)))
        .filter(|(_, distance)| *distance <= limit)
        .collect();
    found.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(b.0)));
    found
}

/// The replacements for the nearest `candidates` (as sorted by [`near`]), at
/// most [`MAX_FIXES`]. Only a single nearest candidate is preferred.
fn replacements(
    candidates: &[(&str, usize)],
    line: usize,
    start_col: usize,
    end_col: usize,
) -> Vec<Replacement> {
    let unique_nearest = match candidates {
        [(_, first), (_, second), ..] => first < second,
        [_] => true,
        [] => false,
    };
    candidates
        .iter()
        .take(MAX_FIXES)
        .enumerate()
        .map(|(index, (name, _))| Replacement {
            line,
            start_col,
            end_col,
            new_text: (*name).to_string(),
            preferred: index == 0 && unique_nearest,
        })
        .collect()
}

/// Where the xref `target` on `line` of `path` points in each top-level
/// document that reads the whole file, with that document's catalog. A
/// document that reads only part of it is no judge of what is missing.
fn whole_readers(
    catalogs: &CatalogCache<'_>,
    path: &Path,
    line: usize,
    target: &str,
) -> Vec<(Rc<AnchorScope>, Result<ResolvedXref, DiagnosticKind>)> {
    catalogs
        .resolve_target(path, line, &Target::Xref(target), Readings::Build)
        .into_iter()
        .filter_map(|resolved| {
            let catalog = catalogs.get(&resolved.reading.document);
            let result = resolved.result.into_xref()?;
            catalog.reads_whole(path).then_some((catalog, result))
        })
        .collect()
}

fn xref_fixes(
    workspace: &Workspace,
    path: &Path,
    diagnostic: &Diagnostic,
    target: &str,
) -> Vec<Replacement> {
    let Some(document) = workspace.documents().get(path) else {
        return Vec::new();
    };
    let Some(xref) = document.xrefs.iter().find(|xref| {
        xref.line == diagnostic.line && xref.span == diagnostic.span && xref.target == target
    }) else {
        return Vec::new();
    };
    let Some((id, start_col, end_col)) = xref.id_span() else {
        return Vec::new();
    };
    // An attribute in the ID is something the user typed on purpose.
    if id.contains('{') {
        return Vec::new();
    }

    let catalogs = CatalogCache::new(workspace);
    // Every ID a typo can be fixed to: the IDs of the catalogs in which the
    // xref is looked up. None where the file part does not resolve, since
    // then the file, not the ID, is the problem.
    let sources: Option<Vec<Rc<AnchorScope>>> = match parse_xref_target(&xref.target).0 {
        None => Some(
            whole_readers(&catalogs, path, xref.line, &xref.target)
                .into_iter()
                .map(|(catalog, _)| catalog)
                .collect(),
        ),
        Some(file) => whole_readers(&catalogs, path, xref.line, file)
            .into_iter()
            .map(|(catalog, result)| {
                let resolved = result.ok()?;
                Some(if catalog.contains_file(&resolved.file) {
                    catalog
                } else {
                    catalogs.get(&resolved.file)
                })
            })
            .collect(),
    };
    let Some(sources) = sources else {
        return Vec::new();
    };
    let ids: BTreeSet<String> = sources
        .iter()
        .flat_map(|source| source.ids().map(|(id, _)| id.to_string()))
        .collect();

    // A fix that leaves the diagnostic in another document is none.
    let prefix = &xref.target[..xref.target.len() - id.len()];
    let resolves_everywhere = |candidate: &str| {
        whole_readers(&catalogs, path, xref.line, &format!("{prefix}{candidate}"))
            .iter()
            .all(|(_, result)| result.is_ok())
    };
    let nearby: Vec<(&str, usize)> = near(id, ids.iter().map(String::as_str))
        .into_iter()
        .filter(|(candidate, _)| resolves_everywhere(candidate))
        .collect();
    replacements(&nearby, xref.line, start_col, end_col)
}

fn include_fixes(
    workspace: &Workspace,
    path: &Path,
    diagnostic: &Diagnostic,
    written: &str,
) -> Vec<Replacement> {
    let Some(document) = workspace.documents().get(path) else {
        return Vec::new();
    };
    let Some(include) = document
        .includes
        .iter()
        .find(|include| include.line == diagnostic.line && include.path == written)
    else {
        return Vec::new();
    };
    // The folder the target is looked for in, per top-level document that
    // reads the file, as go-to-definition resolves it.
    let readings = CatalogCache::new(workspace).resolve_target(
        path,
        include.line,
        &Target::Include(&include.path),
        Readings::Build,
    );
    let resolved: Vec<PathBuf> = readings
        .into_iter()
        .filter_map(|reading| reading.result.into_include())
        .map(|include| include.joined)
        .collect();
    // Only the last segment is replaced, and only when it is the file name
    // as written: an attribute in it would be overwritten.
    let segment_start = include.path.rfind('/').map_or(0, |slash| slash + 1);
    let name = &include.path[segment_start..];
    let folders: Vec<&Path> = resolved
        .iter()
        .filter_map(|joined| {
            let file = joined.file_name()?.to_str()?;
            (file == name).then(|| joined.parent()).flatten()
        })
        .collect();
    if resolved.is_empty() || folders.len() != resolved.len() {
        return Vec::new();
    }
    let including = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let files: BTreeSet<String> = folders
        .iter()
        .filter_map(|folder| std::fs::read_dir(folder).ok())
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|entry| {
            entry.is_file() && entry.canonicalize().map_or(true, |real| real != including)
        })
        .filter_map(|entry| entry.file_name()?.to_str().map(str::to_string))
        .filter(|file| !file.starts_with('.'))
        .collect();
    // A fix that leaves the diagnostic in another document is none.
    let nearby: Vec<(&str, usize)> = near(name, files.iter().map(String::as_str))
        .into_iter()
        .filter(|(candidate, _)| {
            folders.iter().all(|folder| {
                let file = folder.join(candidate);
                file.is_file() && file.canonicalize().map_or(true, |real| real != including)
            })
        })
        .collect();
    replacements(
        &nearby,
        include.line,
        include.target_col + segment_start,
        include.target_col + include.path.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DiagnosticKind;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn fixture_workspace() -> (PathBuf, Workspace) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/quick_fixes")
            .canonicalize()
            .unwrap();
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();
        (root, ws)
    }

    /// The (line, byte column) of the first `needle` in `file`.
    fn locate(file: &Path, needle: &str) -> (usize, usize) {
        let text = std::fs::read_to_string(file).unwrap();
        let offset = text.find(needle).unwrap();
        let line = text[..offset].matches('\n').count();
        let col = offset - text[..offset].rfind('\n').map_or(0, |i| i + 1);
        (line + 1, col)
    }

    /// The diagnostic of `file` on the line of `needle` that covers it: an
    /// xref by span, an include by its whole line.
    fn diagnostic_at<'a>(ws: &'a Workspace, file: &Path, needle: &str) -> &'a Diagnostic {
        let (line, col) = locate(file, needle);
        ws.diagnostics_for(file)
            .iter()
            .find(|d| {
                d.line == line && d.span.is_none_or(|s| s.start_col <= col && col < s.end_col)
            })
            .unwrap_or_else(|| panic!("no diagnostic at {needle:?}"))
    }

    /// `(new_text, preferred, (start_col, end_col))` of every fix for the
    /// diagnostic at `needle`.
    fn fixes_at(ws: &Workspace, file: &Path, needle: &str) -> Vec<(String, bool, (usize, usize))> {
        let diagnostic = diagnostic_at(ws, file, needle);
        quick_fixes(ws, file, diagnostic)
            .into_iter()
            .map(|r| {
                assert_eq!(r.line, diagnostic.line);
                (r.new_text, r.preferred, (r.start_col, r.end_col))
            })
            .collect()
    }

    fn range_of(file: &Path, needle: &str, inner: &str) -> (usize, usize) {
        let (_, col) = locate(file, needle);
        let start = col + needle.find(inner).unwrap();
        (start, start + inner.len())
    }

    fn names(fixes: &[(String, bool, (usize, usize))]) -> Vec<&str> {
        fixes.iter().map(|(name, _, _)| name.as_str()).collect()
    }

    /// Asciidoctor (`main.adoc`): `INFO: possible invalid reference:
    /// setup-instal`; `fixed` with `setup-install`: no message.
    #[test]
    fn test_broken_xref_suggests_nearest_anchor_id() {
        let (root, ws) = fixture_workspace();
        let main = root.join("main.adoc");

        let fixes = fixes_at(&ws, &main, "<<setup-instal>>");

        assert_eq!(
            fixes,
            vec![(
                "setup-install".to_string(),
                true,
                range_of(&main, "<<setup-instal>>", "setup-instal")
            )]
        );
    }

    /// Asciidoctor (`main.adoc`): `INFO: possible invalid reference:
    /// Setup-Install`. IDs are case-sensitive.
    #[test]
    fn test_case_typo_suggests_anchor_ids_are_case_sensitive() {
        let (root, ws) = fixture_workspace();

        let fixes = fixes_at(&ws, &root.join("main.adoc"), "<<Setup-Install>>");

        assert_eq!(names(&fixes), vec!["setup-install"]);
    }

    #[test]
    fn test_long_form_xref_fix_keeps_the_link_text() {
        let (root, ws) = fixture_workspace();
        let main = root.join("main.adoc");

        let long = fixes_at(&ws, &main, "xref:setup-instal[here]");
        let with_text = fixes_at(&ws, &main, "<<setup-instal,the install step>>");

        assert_eq!(
            long[0].2,
            range_of(&main, "xref:setup-instal[here]", "setup-instal")
        );
        assert_eq!(
            with_text[0].2,
            range_of(&main, "<<setup-instal,the", "setup-instal")
        );
    }

    /// Asciidoctor (`main.adoc`): `possible invalid reference: chapter-intr`;
    /// the file part is internal, only the ID is replaced.
    #[test]
    fn test_file_anchor_xref_fix_replaces_only_the_id() {
        let (root, ws) = fixture_workspace();
        let main = root.join("main.adoc");

        let fixes = fixes_at(&ws, &main, "xref:chapter.adoc#chapter-intr[intro]");

        assert_eq!(names(&fixes), vec!["chapter-intro"]);
        assert_eq!(
            fixes[0].2,
            range_of(
                &main,
                "xref:chapter.adoc#chapter-intr[intro]",
                "chapter-intr"
            )
        );
    }

    /// Asciidoctor (`main.adoc`): `possible invalid reference: _configur`;
    /// `_configure` is the implicit ID of `=== Configure`.
    #[test]
    fn test_implicit_section_id_is_a_candidate() {
        let (root, ws) = fixture_workspace();

        let fixes = fixes_at(&ws, &root.join("main.adoc"), "<<_configur>>");

        assert_eq!(names(&fixes), vec!["_configure"]);
    }

    #[test]
    fn test_equal_distances_cap_at_three_without_preferred() {
        let (root, ws) = fixture_workspace();

        let fixes = fixes_at(&ws, &root.join("main.adoc"), "<<part-e>>");

        assert_eq!(names(&fixes), vec!["part-a", "part-b", "part-c"]);
        assert!(fixes.iter().all(|(_, preferred, _)| !preferred));
    }

    #[test]
    fn test_no_fix_without_a_close_anchor() {
        let (root, ws) = fixture_workspace();

        assert_eq!(
            fixes_at(&ws, &root.join("main.adoc"), "<<zzz-nothing>>"),
            vec![]
        );
    }

    /// The file part is the problem, not the ID: out of scope.
    #[test]
    fn test_no_xref_fix_when_the_file_part_is_broken() {
        let (root, ws) = fixture_workspace();

        assert_eq!(
            fixes_at(
                &ws,
                &root.join("main.adoc"),
                "xref:chaptr.adoc#chapter-intro[]"
            ),
            vec![]
        );
    }

    /// Asciidoctor (`main.adoc`): `possible invalid reference:
    /// {prefix}-instal`. The attribute is typed by the user, no fix.
    #[test]
    fn test_no_xref_fix_for_an_id_with_an_attribute_reference() {
        let (root, ws) = fixture_workspace();

        assert_eq!(
            fixes_at(&ws, &root.join("main.adoc"), "<<{prefix}-instal>>"),
            vec![]
        );
    }

    /// Asciidoctor (`main.adoc`): `possible invalid reference: {x}setup-install`.
    /// `setup-install` is within the typo distance, but the attribute is
    /// typed by the user, so the guard alone decides.
    #[test]
    fn test_no_xref_fix_for_an_attribute_reference_next_to_a_close_id() {
        let (root, ws) = fixture_workspace();

        assert_eq!(
            fixes_at(&ws, &root.join("main.adoc"), "<<{x}setup-install>>"),
            vec![]
        );
    }

    /// `other.adoc` is not included by `main.adoc`; the IDs of the file part
    /// are the candidates, not those of `main.adoc`.
    #[test]
    fn test_separate_file_xref_fix_uses_the_ids_of_that_file() {
        let (root, ws) = fixture_workspace();
        let main = root.join("main.adoc");

        let fixes = fixes_at(&ws, &main, "xref:other.adoc#other-intr[]");

        assert_eq!(names(&fixes), vec!["other-intro"]);
        assert_eq!(
            fixes[0].2,
            range_of(&main, "xref:other.adoc#other-intr[]", "other-intr")
        );
    }

    #[test]
    fn test_separate_file_xref_does_not_offer_ids_of_the_including_file() {
        let (root, ws) = fixture_workspace();

        assert_eq!(
            fixes_at(
                &ws,
                &root.join("main.adoc"),
                "xref:other.adoc#setup-instal[]"
            ),
            vec![]
        );
    }

    /// Asciidoctor: `book.adoc` and `web.adoc` both report `setp` and
    /// `instal`; `setup` exists in both, `install` only in `book.adoc`, so
    /// `instal` -> `install` would leave the message in `web.adoc`.
    #[test]
    fn test_shared_file_fix_must_resolve_in_every_reading_document() {
        let (root, ws) = fixture_workspace();
        let shared = root.join("shared/shared.adoc");

        assert_eq!(names(&fixes_at(&ws, &shared, "<<setp>>")), vec!["setup"]);
        assert_eq!(fixes_at(&ws, &shared, "<<instal>>"), vec![]);
    }

    /// Asciidoctor (`main.adoc`): `ERROR: include file not found: chaptr.adoc`.
    #[test]
    fn test_broken_include_suggests_nearest_file_name() {
        let (root, ws) = fixture_workspace();
        let main = root.join("main.adoc");

        let fixes = fixes_at(&ws, &main, "include::chaptr.adoc[]");

        assert_eq!(
            fixes,
            vec![(
                "chapter.adoc".to_string(),
                true,
                range_of(&main, "include::chaptr.adoc[]", "chaptr.adoc")
            )]
        );
    }

    /// Asciidoctor (`main.adoc`): `INFO: optional include dropped because
    /// include file not found: chaptre.adoc`. A typo stays one when the
    /// include is optional.
    #[test]
    fn test_optional_include_suggests_nearest_file_name() {
        let (root, ws) = fixture_workspace();
        let main = root.join("main.adoc");

        let fixes = fixes_at(&ws, &main, "include::chaptre.adoc[opts=optional]");

        assert_eq!(
            fixes,
            vec![(
                "chapter.adoc".to_string(),
                true,
                range_of(
                    &main,
                    "include::chaptre.adoc[opts=optional]",
                    "chaptre.adoc"
                )
            )]
        );
    }

    /// Asciidoctor (`main.adoc`): `include file not found:
    /// snippets/config.yaml`.
    #[test]
    fn test_include_fix_offers_non_adoc_files_and_keeps_the_directory() {
        let (root, ws) = fixture_workspace();
        let main = root.join("main.adoc");

        let fixes = fixes_at(&ws, &main, "include::snippets/config.yaml[]");

        assert_eq!(names(&fixes), vec!["config.yml"]);
        assert_eq!(
            fixes[0].2,
            range_of(&main, "include::snippets/config.yaml[]", "config.yaml")
        );
    }

    /// Asciidoctor (`attrs.adoc`): `include::{snippetsdir}/config.yaml[]`
    /// is `include file not found`.
    #[test]
    fn test_include_fix_keeps_an_attribute_in_the_directory() {
        let (root, ws) = fixture_workspace();
        let main = root.join("main.adoc");

        let fixes = fixes_at(&ws, &main, "include::{snippetsdir}/config.yaml[]");

        assert_eq!(names(&fixes), vec!["config.yml"]);
        assert_eq!(
            fixes[0].2,
            range_of(&main, "include::{snippetsdir}/config.yaml[]", "config.yaml")
        );
    }

    #[test]
    fn test_no_include_fix_for_an_attribute_in_the_file_name() {
        let (root, ws) = fixture_workspace();

        assert_eq!(
            fixes_at(&ws, &root.join("main.adoc"), "include::{name}.adoc[]"),
            vec![]
        );
    }

    #[test]
    fn test_no_include_fix_when_the_directory_is_missing() {
        let (root, ws) = fixture_workspace();

        assert_eq!(
            fixes_at(
                &ws,
                &root.join("main.adoc"),
                "include::snipets/config.yml[]"
            ),
            vec![]
        );
    }

    /// `snippets/.notes.yml` is at distance 1 but hidden.
    #[test]
    fn test_include_fix_skips_hidden_files() {
        let (root, ws) = fixture_workspace();

        assert_eq!(
            fixes_at(
                &ws,
                &root.join("main.adoc"),
                "include::snippets/notes.yml[]"
            ),
            vec![]
        );
    }

    /// Two top-level documents read the include with a different folder:
    /// a candidate is a fix only where it resolves in each of them.
    #[test]
    fn test_include_fix_offers_only_candidates_that_resolve_in_every_reading() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for (path, content) in [
            ("book.adoc", "= Book\n:d: a\n\ninclude::part.adoc[]\n"),
            ("web.adoc", "= Web\n:d: b\n\ninclude::part.adoc[]\n"),
            ("part.adoc", "include::{d}/chaptr.adoc[]\n"),
            ("a/chapter.adoc", "A\n"),
            ("a/chaptor.adoc", "A\n"),
            ("b/chapter.adoc", "B\n"),
        ] {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();
        let part = root.join("part.adoc");

        let fixes = fixes_at(&ws, &part, "include::{d}/chaptr.adoc[]");

        assert_eq!(names(&fixes), vec!["chapter.adoc"]);
    }

    #[test]
    fn test_include_fix_never_suggests_a_self_include() {
        let (root, ws) = fixture_workspace();

        let fixes = fixes_at(&ws, &root.join("main.adoc"), "include::man.adoc[]");

        assert!(!names(&fixes).contains(&"main.adoc"), "{fixes:?}");
    }

    /// The core promise: after the replacement the diagnostic is gone.
    #[test]
    fn test_applied_fix_clears_the_diagnostic() {
        let (root, mut ws) = fixture_workspace();
        let main = root.join("main.adoc");
        let fixes = fixes_at(&ws, &main, "<<setup-instal>>");
        let (line, _) = locate(&main, "<<setup-instal>>");
        let (new_text, _, (start, end)) = &fixes[0];
        let text = std::fs::read_to_string(&main).unwrap();
        let fixed: Vec<String> = text
            .lines()
            .enumerate()
            .map(|(i, l)| {
                if i + 1 == line {
                    format!("{}{}{}", &l[..*start], new_text, &l[*end..])
                } else {
                    l.to_string()
                }
            })
            .collect();
        let overrides = HashMap::from([(main.clone(), Arc::new(fixed.join("\n") + "\n"))]);

        ws.update_file(&main, &overrides);
        ws.recalculate_diagnostics();

        let broken: Vec<&str> = ws
            .diagnostics_for(&main)
            .iter()
            .filter(|d| d.line == line)
            .filter_map(|d| match &d.kind {
                DiagnosticKind::BrokenXref { target, .. } => Some(target.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(broken, vec!["Setup-Install", "setup-instal"]);
    }
}
