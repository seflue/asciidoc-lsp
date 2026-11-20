//! Goto and the diagnostics agree on every target of the project's own
//! documents: where go-to-definition finds an xref, image or include, the
//! diagnostics report it as missing nowhere, and where it finds nothing they
//! report it. Both resolve the same macro per reading (ADR-008), but the
//! rule lives in two places and is kept equal by hand; this test pins it.
//!
//! The corpus is every directory in `tests/fixtures/` and `docs/`, each
//! scanned as a workspace of its own.
//!
//! Left out, as the diagnostics leave them out:
//! - xrefs and images no whole reader judges: a document that reads the file
//!   only in part is no judge of what is missing there;
//! - includes with a URI target and optional includes.
//!
//! A deviation that is not listed in [`KNOWN_DEVIATIONS`] fails the test,
//! and so does a listed one that no longer occurs.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use asciidoc_core::anchor_scope::CatalogCache;
use asciidoc_core::diagnostics::collect_diagnostics;
use asciidoc_core::paths::is_uri;
use asciidoc_core::types::{Diagnostic, DiagnosticKind};
use asciidoc_core::workspace::Workspace;

use crate::navigation::{find_node_at_position, resolve_goto_definitions, NodeKind};
use crate::positions;

/// The kind of target a deviation is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Xref,
    Image,
    Include,
}

/// A known deviation: corpus directory (relative to the repository root),
/// file (relative to that directory), 1-based line, kind, and the cause.
struct Known {
    corpus: &'static str,
    file: &'static str,
    line: usize,
    kind: Kind,
    cause: &'static str,
}

/// Deviations the test accepts, each with its cause. The includes shrink to
/// nothing when the include diagnostic judges per reading like goto.
const KNOWN_DEVIATIONS: &[Known] = &[
    Known {
        corpus: "tests/fixtures/docdir_top_level",
        file: "docs/shared/chapters.adoc",
        line: 1,
        kind: Kind::Include,
        cause: "the registry substitutes {docdir} with the folder of the including \
                file and misses the target; the build reading takes the docdir of \
                its top-level document and finds it (registry against reading)",
    },
    Known {
        corpus: "tests/fixtures/paths_in_include_preview",
        file: "chapters/chapter.adoc",
        line: 14,
        kind: Kind::Include,
        cause: "the registry substitutes {docdir} with the folder of the including \
                file and misses the target; the build reading takes the docdir of \
                its top-level document and finds it (registry against reading)",
    },
    Known {
        corpus: "tests/fixtures/paths_in_include_preview",
        file: "chapters/chapter.adoc",
        line: 16,
        kind: Kind::Include,
        cause: "the registry substitutes {docdir} with the folder of the including \
                file and finds the target; no build reading does, only the \
                preview (registry against reading)",
    },
];

#[derive(Default)]
struct Counts {
    xrefs: usize,
    images: usize,
    includes: usize,
}

/// Corpus directories, sorted: `tests/fixtures/*` and `docs`.
fn corpus_roots() -> Vec<PathBuf> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut roots: Vec<PathBuf> = fs::read_dir(repo.join("tests/fixtures"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect();
    roots.push(repo.join("docs"));
    roots.sort();
    roots
        .into_iter()
        .map(|root| root.canonicalize().unwrap())
        .collect()
}

fn diagnostics_at<'a>(
    diags: &'a HashMap<PathBuf, Vec<Diagnostic>>,
    path: &Path,
    line: usize,
) -> impl Iterator<Item = &'a DiagnosticKind> {
    diags
        .get(path)
        .into_iter()
        .flatten()
        .filter(move |diag| diag.line == line)
        .map(|diag| &diag.kind)
}

/// Every deviation in the workspace at `root`, as (relative file, line, kind,
/// description); `counts` gets the number of targets checked.
fn deviations_in(root: &Path, counts: &mut Counts) -> Vec<(String, usize, Kind, String)> {
    let mut ws = Workspace::new(root);
    ws.scan_workspace();
    let diags = collect_diagnostics(root, &ws);
    let catalogs = CatalogCache::new(&ws);
    let mut found = Vec::new();

    let mut paths: Vec<&PathBuf> = ws.documents().keys().collect();
    paths.sort();
    for path in paths {
        let doc = &ws.documents()[path];
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();
        let goto = |line: usize, col: usize| {
            let pos = positions::to_lsp_position(line, col, &content);
            resolve_goto_definitions(&content, pos, path, Some(&ws))
        };
        let node_at = |line: usize, col: usize| {
            find_node_at_position(&content, positions::to_lsp_position(line, col, &content))
                .map(|info| info.kind)
        };
        let readers = catalogs.scopes_at(path);
        let judged = readers.iter().any(|scope| scope.reads_whole(path));

        for xref in &doc.xrefs {
            counts.xrefs += 1;
            let col = xref.span.map_or(xref.target_col, |span| span.start_col);
            let found_here = goto(xref.line, col);
            let located = matches!(
                node_at(xref.line, col),
                Some(NodeKind::Xref { target } | NodeKind::ShorthandXref { target })
                    if target == xref.target
            );
            // The number of documents the diagnostic names; none when one
            // reading judges alone.
            let broken = diagnostics_at(&diags, path, xref.line).find_map(|kind| match kind {
                DiagnosticKind::BrokenXref { target, documents } if *target == xref.target => {
                    Some(documents.len())
                }
                _ => None,
            });
            let problem = if !located {
                Some("goto does not recognise the xref at its span".to_string())
            } else if !found_here.is_empty() && broken == Some(0) {
                Some(format!("goto finds {found_here:?}, diagnostic says broken"))
            } else if found_here.is_empty() && broken.is_none() && judged {
                Some("goto finds nothing, no diagnostic".to_string())
            } else {
                None
            };
            if let Some(problem) = problem {
                found.push((
                    rel.clone(),
                    xref.line,
                    Kind::Xref,
                    format!("{} : {problem}", xref.target),
                ));
            }
        }

        for image in &doc.images {
            counts.images += 1;
            let found_here = goto(image.line, image.span.start_col);
            let located = matches!(
                node_at(image.line, image.span.start_col),
                Some(NodeKind::Image { path, .. }) if path == image.path
            );
            let missing = diagnostics_at(&diags, path, image.line).any(|kind| {
                matches!(kind, DiagnosticKind::ImageNotFound { path, .. } if *path == image.path)
            });
            let builds = ws.scopes_at(path, image.line);
            let preview = ws.preview_scope_at(path, image.line);
            let several = builds.len() + usize::from(preview.is_some()) > 1;
            // The readings the diagnostics judge, whole readers and preview,
            // and whether any of them can say a file is missing: not for a
            // URI or a reference the reading leaves unresolved.
            let can_judge = builds
                .iter()
                .filter(|scope| {
                    readers
                        .iter()
                        .any(|r| r.document() == scope.document() && r.reads_whole(path))
                })
                .chain(preview.iter())
                .any(|scope| {
                    let target = scope.substitute(&image.path);
                    let imagesdir = scope.value("imagesdir").unwrap_or_default();
                    !(target.contains('{') || is_uri(&target) || is_uri(&imagesdir))
                });
            let problem = if !located {
                Some("goto does not recognise the image at its span".to_string())
            } else if !found_here.is_empty() && missing && !several {
                Some(format!(
                    "goto finds {found_here:?}, diagnostic says missing"
                ))
            } else if found_here.is_empty() && !missing && can_judge {
                Some("goto finds nothing, no diagnostic".to_string())
            } else {
                None
            };
            if let Some(problem) = problem {
                found.push((
                    rel.clone(),
                    image.line,
                    Kind::Image,
                    format!("{} : {problem}", image.path),
                ));
            }
        }

        for inc in &doc.includes {
            let kinds: Vec<&DiagnosticKind> = diagnostics_at(&diags, path, inc.line).collect();
            let optional = kinds.iter().any(|kind| {
                matches!(kind, DiagnosticKind::OptionalIncludeNotFound { path } if *path == inc.path)
            });
            let uri = ws
                .scopes_at(path, inc.line)
                .iter()
                .any(|scope| is_uri(&scope.substitute(&inc.path)));
            if optional || uri {
                continue;
            }
            counts.includes += 1;
            let found_here = goto(inc.line, 0);
            let located = matches!(
                node_at(inc.line, 0),
                Some(NodeKind::Include { path }) if path == inc.path
            );
            let broken = kinds.iter().any(
                |kind| matches!(kind, DiagnosticKind::BrokenInclude { path } if *path == inc.path),
            );
            let problem = if !located {
                Some("goto does not recognise the include".to_string())
            } else if !found_here.is_empty() && broken {
                Some(format!("goto finds {found_here:?}, diagnostic says broken"))
            } else if found_here.is_empty() && !broken {
                Some("goto finds nothing, no diagnostic".to_string())
            } else {
                None
            };
            if let Some(problem) = problem {
                found.push((
                    rel.clone(),
                    inc.line,
                    Kind::Include,
                    format!("{} : {problem}", inc.path),
                ));
            }
        }
    }
    found
}

#[test]
fn test_goto_and_diagnostics_agree_on_every_target() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let mut counts = Counts::default();
    let mut actual: BTreeSet<(String, String, usize, Kind)> = BTreeSet::new();
    let mut described = Vec::new();
    for root in corpus_roots() {
        let corpus = root.strip_prefix(&repo).unwrap().display().to_string();
        for (file, line, kind, text) in deviations_in(&root, &mut counts) {
            actual.insert((corpus.clone(), file.clone(), line, kind));
            described.push(format!("{corpus}/{file}:{line} {kind:?} {text}"));
        }
    }
    eprintln!(
        "checked {} xrefs, {} images, {} includes",
        counts.xrefs, counts.images, counts.includes
    );
    let expected: BTreeSet<(String, String, usize, Kind)> = KNOWN_DEVIATIONS
        .iter()
        .map(|known| {
            (
                known.corpus.to_string(),
                known.file.to_string(),
                known.line,
                known.kind,
            )
        })
        .collect();
    let unexpected: Vec<String> = described
        .iter()
        .filter(|text| {
            !KNOWN_DEVIATIONS.iter().any(|known| {
                text.starts_with(&format!(
                    "{}/{}:{} {:?} ",
                    known.corpus, known.file, known.line, known.kind
                ))
            })
        })
        .cloned()
        .collect();
    assert!(
        unexpected.is_empty(),
        "unexpected deviations:\n{}",
        unexpected.join("\n")
    );
    let stale: Vec<_> = expected.difference(&actual).collect();
    assert!(
        stale.is_empty(),
        "known deviations that no longer occur: {stale:?}"
    );
    assert!(KNOWN_DEVIATIONS.iter().all(|known| !known.cause.is_empty()));
}
