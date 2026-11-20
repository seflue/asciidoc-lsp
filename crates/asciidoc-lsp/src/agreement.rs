//! Goto and the diagnostics agree on every target of the project's own
//! documents: where go-to-definition finds an xref, image or include, the
//! diagnostics report it as missing nowhere, and where it finds nothing they
//! report it. Both ask `resolve_target` per reading (ADR-008); this test pins
//! that they present its answer alike.
//!
//! The corpus is every directory in `tests/fixtures/` and `docs/`, each
//! scanned as a workspace of its own.
//!
//! Where several documents read a file, the diagnostic names exactly the
//! documents whose reading of the target fails, as `resolve_target` resolves
//! it per reading. Optional includes are compared with their own
//! diagnostic.
//!
//! A reading that reads the file only in part counts neither as missing nor
//! as found; where only such readings exist, neither goto nor the
//! diagnostics report anything.
//!
//! Left out, as the diagnostics leave them out: includes with a URI target.
//!
//! Any deviation fails the test.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use asciidoc_core::anchor_scope::CatalogCache;
use asciidoc_core::diagnostics::collect_diagnostics;
use asciidoc_core::targets::{ImageOutcome, IncludeResult, Readings, Target, TargetResult};
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

/// The documents a diagnostic names for the readings in `missing`: none when
/// nothing is missing (`None`), none named when one document reads the file
/// alone, else those that miss the target, relative to `root`.
fn expected_names(root: &Path, missing: &[&Path], readers: usize) -> Option<Vec<PathBuf>> {
    if missing.is_empty() {
        return None;
    }
    Some(if readers > 1 {
        missing
            .iter()
            .map(|document| {
                document
                    .strip_prefix(root)
                    .unwrap_or(document)
                    .to_path_buf()
            })
            .collect()
    } else {
        Vec::new()
    })
}

/// Whether an include is marked optional, read from its attributes: an
/// `optional-option`, or `optional` in the `opts` list, quotes removed.
/// Deliberately written apart from the rule in `diagnostics`, so that this
/// test does not share a mistake with the code it checks.
fn is_optional(attributes: &[(String, String)], substitute: impl Fn(&str) -> String) -> bool {
    attributes.iter().any(|(name, value)| {
        name == "optional-option"
            || (name == "opts"
                && substitute(value)
                    .trim_matches(['"', '\''])
                    .split(',')
                    .any(|option| option.trim() == "optional"))
    })
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

        for xref in &doc.xrefs {
            counts.xrefs += 1;
            let col = xref.span.map_or(xref.target_col, |span| span.start_col);
            let found_here = goto(xref.line, col);
            let located = matches!(
                node_at(xref.line, col),
                Some(NodeKind::Xref { target } | NodeKind::ShorthandXref { target })
                    if target == xref.target
            );
            // What the build readings say: goto follows those that find the
            // target, the diagnostic names those that miss it (all documents
            // that read the file, when several do).
            let resolved = catalogs.resolve_target(
                path,
                xref.line,
                &Target::Xref(&xref.target),
                Readings::Build,
            );
            let missing: Vec<&Path> = resolved
                .iter()
                .filter(|resolved| matches!(&resolved.result, TargetResult::Xref(Err(_))))
                .map(|resolved| resolved.reading.document.as_path())
                .collect();
            let found_by_a_build = resolved
                .iter()
                .any(|resolved| matches!(&resolved.result, TargetResult::Xref(Ok(_))));
            let named = diagnostics_at(&diags, path, xref.line).find_map(|kind| match kind {
                DiagnosticKind::BrokenXref { target, documents } if *target == xref.target => {
                    Some(documents.clone())
                }
                _ => None,
            });
            let expected = expected_names(root, &missing, readers.len());
            let problem = if !located {
                Some("goto does not recognise the xref at its span".to_string())
            } else if found_here.is_empty() == found_by_a_build {
                Some(format!(
                    "goto finds {found_here:?}, a build reading {} it",
                    if found_by_a_build {
                        "finds"
                    } else {
                        "does not find"
                    }
                ))
            } else if named != expected {
                Some(format!(
                    "diagnostic names {named:?}, the readings that miss it are {expected:?}"
                ))
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
            // What the readings say, as `targets` resolves the image: goto
            // follows the build readings that find the file, the diagnostic
            // names the readings, preview included, that miss it.
            let target = Target::Image(&image.path);
            let outcomes = |readings| -> Vec<ImageOutcome> {
                catalogs
                    .resolve_target(path, image.line, &target, readings)
                    .into_iter()
                    .map(|resolved| resolved.result.into_image().unwrap().outcome)
                    .collect()
            };
            let found_by_a_build = outcomes(Readings::Build)
                .iter()
                .any(|outcome| matches!(outcome, ImageOutcome::Found { .. }));
            let missed_by_a_reading = outcomes(Readings::WithPreview)
                .iter()
                .any(|outcome| matches!(outcome, ImageOutcome::Missing { .. }));
            let problem = if !located {
                Some("goto does not recognise the image at its span".to_string())
            } else if found_here.is_empty() == found_by_a_build {
                Some(format!(
                    "goto finds {found_here:?}, a build reading {} it",
                    if found_by_a_build {
                        "finds"
                    } else {
                        "does not find"
                    }
                ))
            } else if missing != missed_by_a_reading {
                Some(format!(
                    "diagnostic says missing: {missing}, a reading misses it: {missed_by_a_reading}"
                ))
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
            let resolved = catalogs.resolve_target(
                path,
                inc.line,
                &Target::Include(&inc.path),
                Readings::Build,
            );
            let includes: Vec<(&Path, IncludeResult)> = resolved
                .iter()
                .filter_map(|resolved| {
                    let include = resolved.result.clone().into_include()?;
                    Some((resolved.reading.document.as_path(), include))
                })
                .collect();
            if includes.iter().any(|(_, include)| include.is_remote()) {
                continue;
            }
            counts.includes += 1;
            let found_here = goto(inc.line, 0);
            let located = matches!(
                node_at(inc.line, 0),
                Some(NodeKind::Include { path }) if path == inc.path
            );
            let scopes = ws.scopes_at(path, inc.line);
            let optional = is_optional(&inc.attributes, |value| {
                scopes
                    .first()
                    .map_or_else(|| value.to_string(), |scope| scope.substitute(value))
            });
            // What the build readings say: goto follows those that find the
            // file, the diagnostic names those that miss it.
            let missing: Vec<&Path> = includes
                .iter()
                .filter(|(_, include)| !include.exists)
                .map(|(document, _)| *document)
                .collect();
            let found_by_a_build = includes.iter().any(|(_, include)| include.exists);
            let kinds: Vec<&DiagnosticKind> = diagnostics_at(&diags, path, inc.line).collect();
            let broken = kinds.iter().find_map(|kind| match kind {
                DiagnosticKind::BrokenInclude { path, documents } if *path == inc.path => {
                    Some(documents.clone())
                }
                _ => None,
            });
            let reported_optional = kinds.iter().any(|kind| {
                matches!(kind, DiagnosticKind::OptionalIncludeNotFound { path } if *path == inc.path)
            });
            let expected = expected_names(root, &missing, readers.len());
            // A missing optional include has its own diagnostic, without
            // the documents.
            let (expected_broken, expected_optional) = match (optional, expected) {
                (true, expected) => (None, expected.is_some()),
                (false, expected) => (expected, false),
            };
            let problem = if !located {
                Some("goto does not recognise the include".to_string())
            } else if found_here.is_empty() == found_by_a_build {
                Some(format!(
                    "goto finds {found_here:?}, a build reading {} it",
                    if found_by_a_build {
                        "finds"
                    } else {
                        "does not find"
                    }
                ))
            } else if reported_optional != expected_optional {
                Some(format!(
                    "optional include: diagnostic says missing: {reported_optional}, readings that miss it: {missing:?}"
                ))
            } else if broken != expected_broken {
                Some(format!(
                    "diagnostic names {broken:?}, the readings that miss it are {expected_broken:?}"
                ))
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
    let mut deviations = Vec::new();
    for root in corpus_roots() {
        let corpus = root.strip_prefix(&repo).unwrap().display().to_string();
        for (file, line, kind, text) in deviations_in(&root, &mut counts) {
            deviations.push(format!("{corpus}/{file}:{line} {kind:?} {text}"));
        }
    }
    eprintln!(
        "checked {} xrefs, {} images, {} includes",
        counts.xrefs, counts.images, counts.includes
    );
    assert!(
        deviations.is_empty(),
        "deviations:\n{}",
        deviations.join("\n")
    );
}
