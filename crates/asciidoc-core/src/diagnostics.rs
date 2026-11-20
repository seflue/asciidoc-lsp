use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::anchor_scope::{AnchorScope, CatalogCache};
use crate::paths::{display_path, is_uri, resolve_doc_target};
use crate::targets::{ImageOutcome, Readings, Target, TargetResult};
use crate::types::{
    Diagnostic, DiagnosticKind, DocumentModel, ImagesdirOrigin, MissingImage, Span,
};
use crate::workspace::Workspace;

/// A duplicate: the second definition (file, line), the ID, and the first.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct DuplicateKey<'a> {
    file: &'a Path,
    line: usize,
    id: &'a str,
    first_file: &'a Path,
    first_line: usize,
}

/// The quote characters that may wrap an include attribute value.
const QUOTES: [char; 2] = ['"', '\''];

/// The tag names an include must find in its target, as Asciidoctor selects
/// them: `lines` beats `tag` beats `tags`, the first present key decides even
/// when empty. Keys arrive trimmed from `extract_block_macro_attrs`.
/// `tag` is one name, `tags` splits at `;` and `,`. Entries are
/// not trimmed; empty ones, negations (`!x`) and wildcards (`*`, `**`) are
/// never looked up. Duplicates are dropped, written order is kept.
///
/// A value that still holds an attribute reference after `substitute` yields
/// no names: the attribute may be set at build time, which the LSP cannot
/// know. Asciidoctor would report `tag '{name}' not found`; no diagnostic is
/// better than a false one.
fn required_tags(
    attributes: &[(String, String)],
    substitute: impl Fn(&str) -> String,
) -> Vec<String> {
    let find = |key: &str| {
        attributes
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    };
    if find("lines").is_some() {
        return Vec::new();
    }
    let Some((value, split)) = find("tag")
        .map(|value| (value, false))
        .or_else(|| find("tags").map(|value| (value, true)))
    else {
        return Vec::new();
    };
    let value = substitute(value);
    let value = value.trim();
    // A value that opens a quote without closing it is not ours to read:
    // no diagnostic beats a false one. An unquoted value ends at its first
    // comma, as in Asciidoctor, which also substitutes before it parses the
    // list.
    let unquoted = QUOTES
        .iter()
        .find_map(|quote| value.strip_prefix(*quote)?.strip_suffix(*quote));
    if unquoted.is_none() && value.starts_with(QUOTES) {
        return Vec::new();
    }
    let value = unquoted.unwrap_or_else(|| value.split_once(',').map_or(value, |(first, _)| first));
    if value.contains('{') {
        return Vec::new();
    }
    let entries: Vec<&str> = if split {
        value.split([';', ',']).collect()
    } else {
        vec![value]
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let is_name = !entry.is_empty() && !entry.starts_with('!') && entry != "*" && entry != "**";
        if is_name && !names.iter().any(|name| name == entry) {
            names.push(entry.to_string());
        }
    }
    names
}

/// Whether an include is marked optional, as Asciidoctor reads it: an
/// `optional-option` attribute, or `optional` in any `opts` list after
/// substitution and without its quotes.
fn is_optional(attributes: &[(String, String)], substitute: impl Fn(&str) -> String) -> bool {
    attributes.iter().any(|(name, value)| {
        if name == "optional-option" {
            return true;
        }
        if name != "opts" {
            return false;
        }
        let value = substitute(value);
        let value = value.trim();
        let list = QUOTES
            .iter()
            .find_map(|quote| value.strip_prefix(*quote)?.strip_suffix(*quote))
            .unwrap_or(value);
        list.split(',').any(|option| option.trim() == "optional")
    })
}

/// The tag names the file at `target` opens, `None` when it cannot be read.
///
/// A `.adoc`/`.asciidoc` target in `documents` is answered from its model:
/// open buffers are the truth there and the file watcher keeps it fresh. Any
/// other target is read from disk, once per run (`cache`); the watcher does
/// not see such files, so a parsed model of one could be stale.
fn declared_tags<'a>(
    target: &Path,
    documents: &'a HashMap<PathBuf, DocumentModel>,
    cache: &'a mut HashMap<PathBuf, Option<Vec<String>>>,
) -> Option<&'a [String]> {
    let is_adoc = target
        .extension()
        .is_some_and(|ext| ext == "adoc" || ext == "asciidoc");
    if let Some(doc) = documents.get(target).filter(|_| is_adoc) {
        return Some(&doc.tags);
    }
    cache
        .entry(target.to_path_buf())
        .or_insert_with(|| {
            crate::source_text::read_adoc_file(target)
                .ok()
                .map(|text| crate::extractors::extract_tag_directives(&text))
        })
        .as_deref()
}

/// Collect all diagnostics across the workspace, grouped by file.
///
/// Paths named inside messages are display paths, see `display_path`:
/// relative to `root` where that is short, absolute otherwise.
#[must_use]
pub fn collect_diagnostics(
    root: &Path,
    workspace: &Workspace,
) -> HashMap<PathBuf, Vec<Diagnostic>> {
    let documents = &workspace.documents;
    let graph = &workspace.include_graph;
    let registry = &workspace.attributes;
    // One catalog per top-level document, read once per recalculation; a
    // file outside every top-level document's reach is added on first use.
    let catalogs = CatalogCache::new(workspace);
    let top_level: Vec<Rc<AnchorScope>> = catalogs.all();
    let mut diags: HashMap<PathBuf, Vec<Diagnostic>> = HashMap::new();
    // The references each top-level document lacks the attribute for, walked
    // once per recalculation.
    let mut undefined_refs: HashMap<PathBuf, HashSet<(PathBuf, usize, usize)>> = HashMap::new();

    // Tag names of include targets read from disk, once per recalculation.
    let mut target_tags: HashMap<PathBuf, Option<Vec<String>>> = HashMap::new();

    for (path, doc) in documents {
        // 1. Broken xrefs, checked in each top-level document that reads the
        // file (the same ones go-to-definition uses): one diagnostic, naming
        // the documents where the target is missing when several read it.
        let reading: Vec<Rc<AnchorScope>> =
            if doc.xrefs.is_empty() && doc.substituted_refs.is_empty() {
                Vec::new()
            } else {
                catalogs.scopes_at(path)
            };
        for xref in &doc.xrefs {
            // A document that reads the file only in part is no judge of what
            // is missing in it, so it counts neither as missing nor as found.
            let missing: Vec<PathBuf> = catalogs
                .resolve_target(
                    path,
                    xref.line,
                    &Target::Xref(&xref.target),
                    Readings::Build,
                )
                .into_iter()
                .filter(|resolved| matches!(&resolved.result, TargetResult::Xref(Err(_))))
                .map(|resolved| display_path(root, &resolved.reading.document))
                .collect();
            if !missing.is_empty() {
                diags.entry(path.clone()).or_default().push(Diagnostic {
                    file: path.clone(),
                    line: xref.line,
                    span: xref.span,
                    kind: DiagnosticKind::BrokenXref {
                        target: xref.target.clone(),
                        documents: if reading.len() > 1 {
                            missing
                        } else {
                            Vec::new()
                        },
                    },
                });
            }
        }

        // 1b. Undefined attributes, per top-level document like the xrefs:
        // one diagnostic per reference, naming the documents that lack the
        // attribute when several read the file.
        for reference in &doc.substituted_refs {
            let position = (path.clone(), reference.line, reference.start_col);
            let missing: Vec<PathBuf> = reading
                .iter()
                .filter(|scope| scope.reads_whole(path))
                .filter(|scope| {
                    undefined_refs
                        .entry(scope.document().to_path_buf())
                        .or_insert_with(|| workspace.undefined_attribute_refs(scope.document()))
                        .contains(&position)
                })
                .map(|scope| display_path(root, scope.document()))
                .collect();
            if !missing.is_empty() {
                diags.entry(path.clone()).or_default().push(Diagnostic {
                    file: path.clone(),
                    line: reference.line,
                    span: Some(Span {
                        start_col: reference.start_col,
                        end_line: reference.line,
                        end_col: reference.end_col,
                    }),
                    kind: DiagnosticKind::UndefinedAttribute {
                        name: reference.name.clone(),
                        documents: if reading.len() > 1 {
                            missing
                        } else {
                            Vec::new()
                        },
                    },
                });
            }
        }

        // 1c. Missing images, per top-level document and, for a file others
        // include, in the editor's preview of it: one diagnostic per macro,
        // naming the builds that miss the file and where their `imagesdir`
        // comes from.
        for image in &doc.images {
            let missing: Vec<MissingImage> = catalogs
                .resolve_target(
                    path,
                    image.line,
                    &Target::Image(&image.path),
                    Readings::WithPreview,
                )
                .into_iter()
                .filter_map(|resolved| {
                    let result = resolved.result.into_image()?;
                    matches!(result.outcome, ImageOutcome::Missing { .. }).then(|| MissingImage {
                        document: display_path(root, &resolved.reading.document),
                        preview: resolved.reading.preview,
                        imagesdir_from: result.imagesdir_from.map(|origin| ImagesdirOrigin {
                            file: display_path(root, &origin.file),
                            line: origin.line,
                        }),
                    })
                })
                .collect();
            if !missing.is_empty() {
                diags.entry(path.clone()).or_default().push(Diagnostic {
                    file: path.clone(),
                    line: image.line,
                    span: Some(image.span),
                    kind: DiagnosticKind::ImageNotFound {
                        path: image.path.clone(),
                        missing,
                    },
                });
            }
        }

        // 2. Broken includes and missing include tags. A broken include is
        // judged in each top-level document that reads the file, with that
        // document's attributes, as go-to-definition does: one diagnostic,
        // naming the documents where the target is missing when several
        // read it.
        for inc in &doc.includes {
            if is_uri(&registry.substitute(path, &inc.path)) {
                continue;
            }
            let readings = catalogs.resolve_target(
                path,
                inc.line,
                &Target::Include(&inc.path),
                Readings::Build,
            );
            let missing: Vec<PathBuf> = readings
                .iter()
                .filter(|resolved| {
                    matches!(&resolved.result, TargetResult::Include(include)
                        if !include.exists && !include.is_remote())
                })
                .map(|resolved| display_path(root, &resolved.reading.document))
                .collect();
            if !missing.is_empty() {
                let written = inc.path.clone();
                let kind = if is_optional(&inc.attributes, |value| registry.substitute(path, value))
                {
                    DiagnosticKind::OptionalIncludeNotFound { path: written }
                } else {
                    DiagnosticKind::BrokenInclude {
                        path: written,
                        documents: if catalogs.scopes_at(path).len() > 1 {
                            missing
                        } else {
                            Vec::new()
                        },
                    }
                };
                diags.entry(path.clone()).or_default().push(Diagnostic {
                    file: path.clone(),
                    line: inc.line,
                    span: None,
                    kind,
                });
                continue;
            }
            let Some(target_path) = resolve_doc_target(path, &inc.path, registry) else {
                continue;
            };
            let required = required_tags(&inc.attributes, |value| registry.substitute(path, value));
            if required.is_empty() {
                continue;
            }
            let Some(declared) = declared_tags(&target_path, documents, &mut target_tags) else {
                continue;
            };
            let tags: Vec<String> = required
                .into_iter()
                .filter(|tag| !declared.contains(tag))
                .collect();
            if !tags.is_empty() {
                diags.entry(path.clone()).or_default().push(Diagnostic {
                    file: path.clone(),
                    line: inc.line,
                    span: None,
                    kind: DiagnosticKind::IncludeTagNotFound {
                        tags,
                        path: inc.path.clone(),
                    },
                });
            }
        }
    }

    // 3. Duplicate anchors, per top-level document: the second definition in
    // reading order. Documents that read the same pair share one diagnostic.
    let mut found: BTreeMap<DuplicateKey<'_>, Vec<PathBuf>> = BTreeMap::new();
    for scope in &top_level {
        for duplicate in scope.duplicates() {
            let key = DuplicateKey {
                file: &duplicate.second.file,
                line: duplicate.second.line,
                id: &duplicate.id,
                first_file: &duplicate.first.file,
                first_line: duplicate.first.line,
            };
            let readers = found.entry(key).or_default();
            // A document that reads the pair more than once is named once.
            if !readers.iter().any(|d| d == scope.document()) {
                readers.push(scope.document().to_path_buf());
            }
        }
    }
    for (key, mut documents) in found {
        documents.sort();
        diags
            .entry(key.file.to_path_buf())
            .or_default()
            .push(Diagnostic {
                file: key.file.to_path_buf(),
                line: key.line,
                span: None,
                kind: DiagnosticKind::DuplicateAnchor {
                    id: key.id.to_string(),
                    first_file: display_path(root, key.first_file),
                    first_line: key.first_line,
                    documents: documents
                        .iter()
                        .map(|document| display_path(root, document))
                        .collect(),
                },
            });
    }

    // 4. Undefined conditional attributes (ifdef/ifndef)
    // Suppressed: ifdef/ifndef attributes are often only defined at build time,
    // producing false-positive noise. The UndefinedConditionalAttr variant is
    // retained in DiagnosticKind for a future typo-detector.

    // 5. Circular includes
    // Rank the converted document for cycles without an outside entry: a
    // level-0 title that no include shifts, best if it also sets `:doctype:`
    // (a part file of a book has a title but not the doctype).
    let rank = |path: &std::path::Path| {
        let Some(doc) = documents.get(path) else {
            return 2;
        };
        let is_header =
            doc.sections.iter().any(|s| s.level == 0) && !graph.is_level_shifted(path, documents);
        match (
            is_header,
            doc.attributes.iter().any(|a| a.name == "doctype"),
        ) {
            (true, true) => 0,
            (true, false) => 1,
            (false, _) => 2,
        }
    };
    for (source, line, target) in graph.detect_cycles(rank) {
        let path_display = target.file_name().map_or_else(
            || target.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        diags.entry(source.clone()).or_default().push(Diagnostic {
            file: source.clone(),
            line,
            span: None,
            kind: DiagnosticKind::CircularInclude {
                path: path_display.clone(),
                cycle_through: display_path(root, &target),
            },
        });
    }

    // 6. PDF theme and font paths
    for mut diag in crate::pdf_theme::collect_pdf_diagnostics(workspace) {
        match &mut diag.kind {
            DiagnosticKind::PdfPathNotFound { path, .. } => *path = pdf_display_path(root, path),
            DiagnosticKind::PdfThemeEntryNotFound {
                theme,
                path,
                searched,
                ..
            } => {
                *theme = pdf_display_path(root, theme);
                *path = pdf_display_path(root, path);
                for dir in searched {
                    *dir = pdf_display_path(root, Path::new(dir.as_str()))
                        .display()
                        .to_string();
                }
            }
            DiagnosticKind::PdfThemeYamlExtension { value } => {
                *value = pdf_display_path(root, Path::new(value.as_str()))
                    .display()
                    .to_string();
            }
            _ => {}
        }
        diags.entry(diag.file.clone()).or_default().push(diag);
    }

    // 7. Section titles out of sequence, per top-level document: one
    // diagnostic per place, naming the documents when several read the file.
    let mut out_of_sequence: BTreeMap<(PathBuf, usize, Vec<usize>, usize), Vec<PathBuf>> =
        BTreeMap::new();
    for scope in &top_level {
        for found in crate::section_levels::out_of_sequence(workspace, scope.document()) {
            let readers = out_of_sequence
                .entry((found.file, found.line, found.expected, found.got))
                .or_default();
            // A document that reads the file more than once is named once.
            if !readers.iter().any(|d| d == scope.document()) {
                readers.push(scope.document().to_path_buf());
            }
        }
    }
    for ((file, line, expected, got), mut documents) in out_of_sequence {
        documents.sort();
        let several = catalogs.scopes_at(&file).len() > 1;
        diags.entry(file.clone()).or_default().push(Diagnostic {
            file,
            line,
            span: None,
            kind: DiagnosticKind::SectionOutOfSequence {
                expected,
                got,
                documents: if several {
                    documents
                        .iter()
                        .map(|document| display_path(root, document))
                        .collect()
                } else {
                    Vec::new()
                },
            },
        });
    }

    // 8. Level 0 titles outside a book, per top-level document, as in 7.
    let mut level_zero: BTreeMap<(PathBuf, usize), Vec<PathBuf>> = BTreeMap::new();
    for scope in &top_level {
        for found in crate::section_levels::level_zero_outside_book(workspace, scope.document()) {
            let readers = level_zero.entry((found.file, found.line)).or_default();
            if !readers.iter().any(|d| d == scope.document()) {
                readers.push(scope.document().to_path_buf());
            }
        }
    }
    for ((file, line), mut documents) in level_zero {
        documents.sort();
        let several = catalogs.scopes_at(&file).len() > 1;
        diags.entry(file.clone()).or_default().push(Diagnostic {
            file,
            line,
            span: None,
            kind: DiagnosticKind::LevelZeroSectionOutsideBook {
                documents: if several {
                    documents
                        .iter()
                        .map(|document| display_path(root, document))
                        .collect()
                } else {
                    Vec::new()
                },
            },
        });
    }

    // 9. Unterminated delimited blocks. Code includes are parsed too, but
    // their lines are verbatim content of the surrounding listing, not
    // delimiters: only `.adoc` files are checked.
    for (path, doc) in documents {
        if path.extension().is_none_or(|ext| ext != "adoc") {
            continue;
        }
        for block in &doc.unterminated_blocks {
            diags.entry(path.clone()).or_default().push(Diagnostic {
                file: path.clone(),
                line: block.line,
                span: None,
                kind: DiagnosticKind::UnterminatedBlock {
                    context: block.context.to_string(),
                    delimiter: block.delimiter.clone(),
                },
            });
        }
    }

    diags
}

/// A PDF value as shown in a message: root-relative only when it lies under
/// `root`, absolute otherwise, so a value the user wrote outside the root is
/// never rewritten into a `..` form. A relative value stays as written.
///
/// Known gap: `root` is the canonical root. A document inside a
/// non-canonical root that is a symlink to a target outside it has its
/// `{docdir}` in the non-canonical form, so a path derived from it is not
/// recognised as under the root and stays absolute.
fn pdf_display_path(root: &Path, path: &Path) -> PathBuf {
    if !path.is_absolute() {
        return path.to_path_buf();
    }
    let path = crate::paths::normalize_lexically(path);
    match path.strip_prefix(root) {
        Ok(rel) => rel.to_path_buf(),
        Err(_) => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attributes::AttributeRegistry;
    use crate::graph::IncludeGraph;
    use crate::types::{
        Anchor, AttributeDef, AttributeRef, DocumentModel, IncludeMacro, Placement, Section, Xref,
    };
    use pretty_assertions::assert_eq;
    use std::path::Path;

    fn make_registry() -> AttributeRegistry {
        AttributeRegistry::new(Path::new("/tmp"))
    }

    /// The diagnostics of a workspace made of `documents`, as `Workspace::build`
    /// would index them.
    fn diagnose(
        documents: HashMap<PathBuf, DocumentModel>,
        graph: IncludeGraph,
        registry: AttributeRegistry,
    ) -> HashMap<PathBuf, Vec<Diagnostic>> {
        diagnose_in(Path::new("/"), documents, graph, registry)
    }

    /// As `diagnose`, with messages shown relative to `root`.
    fn diagnose_in(
        root: &Path,
        documents: HashMap<PathBuf, DocumentModel>,
        graph: IncludeGraph,
        registry: AttributeRegistry,
    ) -> HashMap<PathBuf, Vec<Diagnostic>> {
        let workspace = Workspace::new_with(
            PathBuf::from("/tmp"),
            registry,
            graph,
            documents,
            HashMap::new(),
        );
        collect_diagnostics(root, &workspace)
    }

    /// Flatten per-file diagnostics into a single Vec for simpler test assertions.
    fn flatten(diags: &HashMap<PathBuf, Vec<Diagnostic>>) -> Vec<&Diagnostic> {
        diags.values().flatten().collect()
    }

    #[test]
    fn test_broken_xref_diagnostic() {
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel::test("/a.adoc").with_xrefs(vec![Xref {
                target: "nonexistent".into(),
                display_text: None,
                line: 5,
                span: None,
                target_col: 0,
            }]),
        );

        let graph = IncludeGraph::new();
        let diags = diagnose(documents, graph, registry);
        let flat = flatten(&diags);

        assert_eq!(flat.len(), 1);
        assert!(matches!(
            &flat[0].kind,
            DiagnosticKind::BrokenXref { target, .. } if target == "nonexistent"
        ));
    }

    #[test]
    fn test_broken_include_diagnostic() {
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel::test("/a.adoc").with_includes(vec![IncludeMacro {
                path: "nonexistent.adoc".into(),
                attributes: vec![],
                placement: Placement::Body,
                line: 3,
                target_col: 9,
            }]),
        );

        let graph = IncludeGraph::new();
        let diags = diagnose(documents, graph, registry);

        let flat = flatten(&diags);
        assert!(flat
            .iter()
            .any(|d| matches!(&d.kind, DiagnosticKind::BrokenInclude { .. })));
    }

    #[test]
    fn test_duplicate_anchor_diagnostic_same_file() {
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel::test("/a.adoc").with_anchors(vec![
                Anchor {
                    id: "dup".into(),
                    reftext: None,
                    line: 1,
                },
                Anchor {
                    id: "dup".into(),
                    reftext: None,
                    line: 10,
                },
            ]),
        );

        let graph = IncludeGraph::new();
        let diags = diagnose(documents, graph, registry);

        let flat = flatten(&diags);
        assert!(flat
            .iter()
            .any(|d| matches!(&d.kind, DiagnosticKind::DuplicateAnchor { id, .. } if id == "dup")));
    }

    /// A file sharing only `/` with the root keeps its absolute path: a
    /// `../../..` chain would be longer and unrecognisable.
    #[test]
    fn test_duplicate_anchor_far_outside_root_keeps_absolute_path() {
        let registry = make_registry();
        let far = "/other-tree/note.adoc";
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from(far),
            DocumentModel::test(far).with_anchors(vec![
                Anchor {
                    id: "dup".into(),
                    reftext: None,
                    line: 1,
                },
                Anchor {
                    id: "dup".into(),
                    reftext: None,
                    line: 10,
                },
            ]),
        );
        let graph = IncludeGraph::new();
        let diags = diagnose_in(Path::new("/work/deep/root"), documents, graph, registry);
        let messages: Vec<String> = flatten(&diags).iter().map(|d| d.kind.to_string()).collect();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("first defined in /other-tree/note.adoc:1")),
            "got: {messages:?}"
        );
    }

    #[test]
    fn test_display_path_rules() {
        let root = Path::new("/work/proj");
        assert_eq!(
            display_path(root, Path::new("/work/proj/a/b.adoc")),
            PathBuf::from("a/b.adoc")
        );
        assert_eq!(
            display_path(root, Path::new("/work/shared/x.adoc")),
            PathBuf::from("../shared/x.adoc")
        );
        assert_eq!(
            display_path(root, Path::new("/opt/themes")),
            PathBuf::from("/opt/themes")
        );
        assert_eq!(
            display_path(root, Path::new("rel/x")),
            PathBuf::from("rel/x")
        );
        // Shares a real ancestor with a deep root: a long `../` chain is
        // unrecognisable, so the absolute path stays, however long the
        // shared prefix makes it.
        assert_eq!(
            display_path(Path::new("/a/b/c/d/e/proj"), Path::new("/a/other.adoc")),
            PathBuf::from("/a/other.adoc")
        );
        assert_eq!(
            display_path(
                Path::new("/very/long/shared/ancestor/path/d1/d2/d3/d4"),
                Path::new("/very/long/shared/ancestor/path/x.adoc")
            ),
            PathBuf::from("/very/long/shared/ancestor/path/x.adoc")
        );
        // Three steps up exceeds MAX_PARENT_STEPS.
        assert_eq!(
            display_path(
                Path::new("/work/proj/sub/deeper"),
                Path::new("/work/x/y.adoc")
            ),
            PathBuf::from("/work/x/y.adoc")
        );
        // Two steps up is still shown relative.
        assert_eq!(
            display_path(Path::new("/work/proj/sub"), Path::new("/work/x/y.adoc")),
            PathBuf::from("../../x/y.adoc")
        );
    }

    #[test]
    fn test_display_path_normalizes_inner_parent_segments() {
        assert_eq!(
            display_path(
                Path::new("/work/proj"),
                Path::new("/work/proj/a/../theme/x.yaml")
            ),
            PathBuf::from("theme/x.yaml")
        );
    }

    #[test]
    fn test_no_duplicate_anchor_diagnostic_cross_file() {
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel::test("/a.adoc").with_anchors(vec![Anchor {
                id: "context".into(),
                reftext: None,
                line: 1,
            }]),
        );
        documents.insert(
            PathBuf::from("/b.adoc"),
            DocumentModel::test("/b.adoc").with_anchors(vec![Anchor {
                id: "context".into(),
                reftext: None,
                line: 1,
            }]),
        );

        let graph = IncludeGraph::new();
        let diags = diagnose(documents, graph, registry);

        let flat = flatten(&diags);
        assert!(!flat
            .iter()
            .any(|d| matches!(&d.kind, DiagnosticKind::DuplicateAnchor { .. })));
    }

    #[test]
    fn test_circular_include_diagnostic() {
        let registry = make_registry();
        let a = PathBuf::from("/a.adoc");
        let b = PathBuf::from("/b.adoc");

        let mut documents = HashMap::new();
        documents.insert(
            a.clone(),
            DocumentModel::test("/a.adoc").with_includes(vec![IncludeMacro {
                path: "b.adoc".into(),
                attributes: vec![],
                placement: Placement::Body,
                line: 3,
                target_col: 9,
            }]),
        );
        documents.insert(
            b.clone(),
            DocumentModel::test("/b.adoc").with_includes(vec![IncludeMacro {
                path: "a.adoc".into(),
                attributes: vec![],
                placement: Placement::Body,
                line: 5,
                target_col: 9,
            }]),
        );

        let mut graph = IncludeGraph::new();
        graph.add_edge(
            &a,
            crate::types::IncludeEdge {
                target: b.clone(),
                line: 3,
                attributes: vec![],
                placement: Placement::Body,
            },
        );
        graph.add_edge(
            &b,
            crate::types::IncludeEdge {
                target: a.clone(),
                line: 5,
                attributes: vec![],
                placement: Placement::Body,
            },
        );

        let diags = diagnose(documents, graph, registry);
        let flat = flatten(&diags);

        let circular: Vec<_> = flat
            .iter()
            .filter(|d| matches!(&d.kind, DiagnosticKind::CircularInclude { .. }))
            .collect();
        assert_eq!(
            circular.len(),
            1,
            "Expected 1 CircularInclude diagnostic, got: {circular:?}"
        );
    }

    #[test]
    fn test_include_with_attribute_path_no_false_positive() {
        // Regression test: includes with {attr} paths must be substituted
        // before checking existence (was bug: raw path checked, not resolved)
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create target file on disk, below the top-level document's folder
        let sub = root.join("book/appendix");
        std::fs::create_dir_all(&sub).unwrap();
        let target_file = sub.join("glossary.adoc");
        std::fs::write(&target_file, "= Glossary\n").unwrap();

        // docdir = folder of the top-level document, not the workspace root
        let mut registry = AttributeRegistry::new(root);
        let parent = root.join("book/index.adoc");
        registry.register_file_attributes(&parent, &[]);

        // Parent doc includes {docdir}/appendix/glossary.adoc
        let mut documents = HashMap::new();
        documents.insert(
            parent.clone(),
            DocumentModel::test(parent.to_str().unwrap()).with_includes(vec![IncludeMacro {
                path: "{docdir}/appendix/glossary.adoc".into(),
                attributes: vec![],
                placement: Placement::Body,
                line: 20,
                target_col: 9,
            }]),
        );

        let graph = IncludeGraph::new();
        let diags = diagnose(documents, graph, registry);
        let flat = flatten(&diags);

        let broken_includes: Vec<_> = flat
            .iter()
            .filter(|d| matches!(&d.kind, DiagnosticKind::BrokenInclude { .. }))
            .collect();
        assert!(
            broken_includes.is_empty(),
            "Include with {{docdir}} attribute should not be reported as broken, got: {:?}",
            broken_includes
        );
    }

    #[test]
    fn test_workspace_diagnostics_cover_all_files() {
        let registry = make_registry();
        let mut documents = HashMap::new();

        // Entry file (clean — no diagnostics)
        documents.insert(
            PathBuf::from("/entry.adoc"),
            DocumentModel::test("/entry.adoc")
                .with_sections(vec![Section {
                    level: 1,
                    title: "Entry".into(),
                    line: 1,
                    id: None,
                    is_header: false,
                    style: None,
                    placement: Placement::Body,
                }])
                .with_anchors(vec![Anchor {
                    id: "entry-anchor".into(),
                    reftext: None,
                    line: 3,
                }]),
        );

        // File with broken xref
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel::test("/a.adoc").with_xrefs(vec![Xref {
                target: "nonexistent-target".into(),
                display_text: None,
                line: 2,
                span: None,
                target_col: 0,
            }]),
        );

        // File with broken include
        documents.insert(
            PathBuf::from("/b.adoc"),
            DocumentModel::test("/b.adoc").with_includes(vec![IncludeMacro {
                path: "missing.adoc".into(),
                attributes: vec![],
                placement: Placement::Body,
                line: 4,
                target_col: 9,
            }]),
        );

        let graph = IncludeGraph::new();
        let diags = diagnose(documents, graph, registry);

        // Diagnostics must cover multiple files — HashMap keys are the files
        assert!(
            diags.len() >= 2,
            "Expected diagnostics for at least 2 files, got {:?}",
            diags.keys().collect::<Vec<_>>()
        );
        assert!(diags.contains_key(&PathBuf::from("/a.adoc")));
        assert!(diags.contains_key(&PathBuf::from("/b.adoc")));
    }

    // --- Conditional attribute diagnostics suppressed ---
    // ifdef/ifndef attributes are not diagnosed because they are often
    // only defined at build time, producing false-positive noise.

    #[test]
    fn test_ifdef_undefined_attr_no_diagnostic() {
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel::test("/a.adoc").with_conditionals(vec![
                crate::types::ConditionalDirective {
                    kind: crate::types::ConditionalKind::Ifdef,
                    attributes: vec!["nonexistent".into()],
                    operator: crate::types::ConditionalOperator::Single,
                    line: 5,
                },
            ]),
        );

        let graph = IncludeGraph::new();
        let diags = diagnose(documents, graph, registry);
        let flat = flatten(&diags);

        let cond_diags: Vec<_> = flat
            .iter()
            .filter(|d| matches!(&d.kind, DiagnosticKind::UndefinedConditionalAttr { .. }))
            .collect();
        assert!(
            cond_diags.is_empty(),
            "ifdef with undefined attr should not produce diagnostic, got: {cond_diags:?}"
        );
    }

    #[test]
    fn test_ifndef_undefined_attr_no_diagnostic() {
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel::test("/a.adoc").with_conditionals(vec![
                crate::types::ConditionalDirective {
                    kind: crate::types::ConditionalKind::Ifndef,
                    attributes: vec!["unknown-attr".into()],
                    operator: crate::types::ConditionalOperator::Single,
                    line: 10,
                },
            ]),
        );

        let graph = IncludeGraph::new();
        let diags = diagnose(documents, graph, registry);
        let flat = flatten(&diags);

        let cond_diags: Vec<_> = flat
            .iter()
            .filter(|d| matches!(&d.kind, DiagnosticKind::UndefinedConditionalAttr { .. }))
            .collect();
        assert!(
            cond_diags.is_empty(),
            "ifndef with undefined attr should not produce diagnostic, got: {cond_diags:?}"
        );
    }

    #[test]
    fn test_env_asciidoc_lsp_is_builtin() {
        let registry = make_registry();
        let file = Path::new("/tmp/any.adoc");
        assert!(
            registry.resolve(file, "env-asciidoc-lsp").is_some(),
            "env-asciidoc-lsp should be a builtin attribute"
        );
    }

    #[test]
    fn test_no_false_positives() {
        let registry = make_registry();
        let mut documents = HashMap::new();

        // File with valid anchor
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel::test("/a.adoc")
                .with_sections(vec![Section {
                    level: 1,
                    title: "Valid".into(),
                    line: 1,
                    id: None,
                    is_header: false,
                    style: None,
                    placement: Placement::Body,
                }])
                .with_anchors(vec![Anchor {
                    id: "valid-anchor".into(),
                    reftext: None,
                    line: 5,
                }]),
        );
        // File with xref to that anchor
        documents.insert(
            PathBuf::from("/b.adoc"),
            DocumentModel::test("/b.adoc").with_xrefs(vec![Xref {
                target: "valid-anchor".into(),
                display_text: None,
                line: 2,
                span: None,
                target_col: 0,
            }]),
        );

        // Asciidoctor knows an ID only within the document it converts: `b.adoc`
        // is part of `a.adoc`, so the xref resolves. Without the include it
        // would be `INFO: possible invalid reference` (see the next test).
        let mut graph = IncludeGraph::new();
        graph.add_edge(
            Path::new("/a.adoc"),
            crate::types::IncludeEdge {
                target: PathBuf::from("/b.adoc"),
                line: 7,
                attributes: vec![],
                placement: Placement::Body,
            },
        );
        let diags = diagnose(documents, graph, registry);
        let flat = flatten(&diags);

        assert!(flat.is_empty(), "Expected no diagnostics, got: {:?}", flat);
    }

    /// Asciidoctor on `b.adoc` alone: `INFO: possible invalid reference:
    /// valid-anchor`. An ID of a document nobody includes is no ID of `b.adoc`.
    #[test]
    fn test_xref_to_id_of_unrelated_top_level_document_is_broken() {
        let registry = make_registry();
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel::test("/a.adoc").with_anchors(vec![Anchor {
                id: "valid-anchor".into(),
                reftext: None,
                line: 5,
            }]),
        );
        documents.insert(
            PathBuf::from("/b.adoc"),
            DocumentModel::test("/b.adoc").with_xrefs(vec![Xref {
                target: "valid-anchor".into(),
                display_text: None,
                line: 2,
                span: None,
                target_col: 0,
            }]),
        );

        let diags = diagnose(documents, IncludeGraph::new(), registry);

        assert!(matches!(
            flatten(&diags)[..],
            [Diagnostic {
                kind: DiagnosticKind::BrokenXref { .. },
                ..
            }]
        ));
    }
    // --- Anchor scope per top-level document (fixture `anchor_scope`) ---

    fn fixture_workspace(name: &str) -> (PathBuf, crate::workspace::Workspace) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(name)
            .canonicalize()
            .unwrap();
        let mut ws = crate::workspace::Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();
        (root, ws)
    }

    /// The messages of the image diagnostics of `file`, by line.
    fn image_messages(ws: &crate::workspace::Workspace, file: &Path) -> Vec<(usize, String)> {
        ws.diagnostics_for(file)
            .iter()
            .filter(|d| matches!(d.kind, DiagnosticKind::ImageNotFound { .. }))
            .map(|d| (d.line, d.kind.to_string()))
            .collect()
    }

    /// An include between two images sets `imagesdir`: the image before it
    /// is judged without, the image after it with the directory.
    #[test]
    fn test_image_scope_follows_include_between_images() {
        let (root, ws) = fixture_workspace("image_scopes_includes");

        assert_eq!(
            image_messages(&ws, &root.join("main.adoc")),
            vec![(
                3,
                "Image not found: logo.png (main.adoc: imagesdir not set in this build)"
                    .to_string()
            )]
        );
    }

    /// The reverse: found before the include, missing in the directory the
    /// include sets.
    #[test]
    fn test_image_scope_include_can_make_later_image_missing() {
        let (root, ws) = fixture_workspace("image_scopes_includes");

        assert_eq!(
            image_messages(&ws, &root.join("reverse.adoc")),
            vec![(
                7,
                "Image not found: top.png (reverse.adoc: imagesdir from setdir.adoc:1)".to_string()
            )]
        );
    }

    /// `book.adoc` sets `imagesdir: img`, `web.adoc` does not; both include
    /// `shared.adoc`. The image is missing where `imagesdir` is not set,
    /// and in the preview of the shared file, which sees neither. The
    /// `image::` in the listing block is text and gets nothing.
    #[test]
    fn test_image_missing_in_one_top_level_document_names_it() {
        let (root, ws) = fixture_workspace("image_scopes");

        assert_eq!(
            image_messages(&ws, &root.join("shared.adoc")),
            vec![
                (
                    3,
                    "Image not found: logo.png (web.adoc: imagesdir not set in this build; \
                     shared.adoc (preview): imagesdir not set in this build)"
                        .to_string()
                ),
                (
                    5,
                    "Image not found: logo.png (web.adoc: imagesdir not set in this build; \
                     shared.adoc (preview): imagesdir not set in this build)"
                        .to_string()
                ),
                (
                    7,
                    "Image not found: gone.png (book.adoc: imagesdir from book.adoc:2; \
                     web.adoc: imagesdir not set in this build; \
                     shared.adoc (preview): imagesdir not set in this build)"
                        .to_string()
                ),
            ]
        );
    }

    /// `chapters/shared.adoc` is included by `book.adoc` and `web.adoc` from
    /// the root and sits in a subfolder. An image resolves against the
    /// folder of the top-level document, the preview against the folder of
    /// the file. Asciidoctor reports the same: `logo.png` at line 3 for
    /// `web.adoc` and the preview, at line 7 for the preview alone, and
    /// `only-preview.png` for `book.adoc` and `web.adoc`, not the preview.
    #[test]
    fn test_image_in_included_subfolder_resolves_against_top_level_folder() {
        let (root, ws) = fixture_workspace("image_scopes_subdir");

        assert_eq!(
            image_messages(&ws, &root.join("chapters/shared.adoc")),
            vec![
                (
                    3,
                    "Image not found: logo.png (web.adoc: imagesdir not set in this build; \
                     chapters/shared.adoc (preview): imagesdir not set in this build)"
                        .to_string()
                ),
                (
                    7,
                    "Image not found: logo.png (chapters/shared.adoc (preview): \
                     imagesdir from chapters/setdir.adoc:1)"
                        .to_string()
                ),
                (
                    9,
                    "Image not found: only-preview.png (book.adoc: \
                     imagesdir from chapters/setdir.adoc:1; \
                     web.adoc: imagesdir from chapters/setdir.adoc:1)"
                        .to_string()
                ),
            ]
        );
    }

    /// Found by the only top-level document, missing in the file's own
    /// preview: a warning too, the file's `.asciidoctorconfig` can fix it.
    #[test]
    fn test_image_missing_only_in_preview_is_reported() {
        let (root, ws) = fixture_workspace("image_scopes");

        assert_eq!(
            image_messages(&ws, &root.join("shared2.adoc")),
            vec![(
                3,
                "Image not found: logo.png (shared2.adoc (preview): \
                 imagesdir not set in this build)"
                    .to_string()
            )]
        );
    }

    #[test]
    fn test_image_missing_in_single_document_block_and_inline() {
        let (root, ws) = fixture_workspace("image_scopes");

        assert_eq!(
            image_messages(&ws, &root.join("solo.adoc")),
            vec![
                (
                    3,
                    "Image not found: missing.png (solo.adoc: imagesdir not set in this build)"
                        .to_string()
                ),
                (
                    5,
                    "Image not found: gone.png (solo.adoc: imagesdir not set in this build)"
                        .to_string()
                ),
            ]
        );
    }

    /// Pins: `image::` in a listing block is literal text, `include::` there
    /// stays active.
    #[test]
    fn test_image_in_listing_gets_no_diagnostic_but_include_does() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let main = root.join("main.adoc");
        std::fs::write(
            &main,
            "= T\n\n----\nimage::missing.png[]\ninclude::missing.adoc[]\n----\n\nimage::missing.png[]\n",
        )
        .unwrap();
        let mut ws = crate::workspace::Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();

        let images: Vec<usize> = image_messages(&ws, &main).iter().map(|m| m.0).collect();
        let includes: Vec<usize> = ws
            .diagnostics_for(&main)
            .iter()
            .filter(|d| matches!(d.kind, DiagnosticKind::BrokenInclude { .. }))
            .map(|d| d.line)
            .collect();
        assert_eq!(images, vec![8]);
        assert_eq!(includes, vec![5]);
    }

    /// Image diagnostics of `main.adoc` with `source`, the files in `images`
    /// created next to it, as `(line, message)`.
    fn image_messages_of(source: &str, images: &[&str]) -> Vec<(usize, String)> {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for image in images {
            let path = root.join(image);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "PNG").unwrap();
        }
        let main = root.join("main.adoc");
        std::fs::write(&main, source).unwrap();
        let mut ws = crate::workspace::Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();
        image_messages(&ws, &main)
    }

    /// Asciidoctor's inline image macro takes no target starting with `:`
    /// (`InlineImageMacroRx`), so `image::x[]` inside a line is no image. In
    /// an `a|` cell it is a block image, which the LSP leaves unchecked
    /// rather than reading `:x` as a missing file.
    #[test]
    fn test_block_image_syntax_inside_a_line_is_no_inline_image() {
        let source = "= T\n\n|===\na|image::here.png[]\n|===\n\nText image::here.png[]trailing\n";
        assert_eq!(image_messages_of(source, &["here.png"]), vec![]);
        assert_eq!(image_messages_of(source, &[]), vec![]);
    }

    /// Asciidoctor renders no image in a `[listing]`, `[literal]`,
    /// `[source]` or `[verse]` paragraph (`VERBATIM_STYLES`, checked in
    /// `Parser.next_block` before block macros), nor in a branch of `ifdef`, `ifndef` or
    /// `ifeval`, which the LSP does not evaluate. A `[comment]` paragraph
    /// starting with a block macro still renders the image (oracle:
    /// `asciidoctor -a data-uri` warns for it).
    #[test]
    fn test_no_image_diagnostic_in_verbatim_paragraphs_and_conditionals() {
        let source = "= T\n\n\
                      [listing]\nimage::a.png[]\n\n\
                      [literal]\nimage::b.png[]\n\n\
                      [source,ruby]\nimage::c.png[]\n\n\
                      [literal]\nText image:d.png[] x.\n\n\
                      ifdef::nope[]\nimage::e.png[]\nendif::[]\n\n\
                      ifndef::x[image::f.png[]]\n\n\
                      ifeval::[1 == 2]\nimage::g.png[]\nendif::[]\n\n\
                      [comment]\nimage::h.png[]\n\n\
                      image::i.png[]\n\n\
                      [verse]\nimage::j.png[]\n";
        let lines: Vec<usize> = image_messages_of(source, &[])
            .into_iter()
            .map(|(line, _)| line)
            .collect();
        assert_eq!(lines, vec![26, 28]);
    }

    /// Contexts in which Asciidoctor renders the image and warns when the
    /// file is missing (oracle: `asciidoctor -a data-uri`): the line of the
    /// single message, the files that exist next to `main.adoc`.
    #[rstest::rstest]
    #[case::inline_in_table_cell("= T\n\n|===\n|image:x.png[]\n|===\n", &[], 4)]
    #[case::inline_in_block_title("= T\n\n.Fig image:x.png[]\nText.\n", &[], 3)]
    #[case::inline_in_list_item("= T\n\n* item image:x.png[]\n", &[], 3)]
    #[case::inline_in_footnote("= T\n\nNote.footnote:[image:x.png[]]\n", &[], 3)]
    #[case::block_in_list_continuation("= T\n\n* item\n+\nimage::x.png[]\n", &[], 5)]
    #[case::inline_after_block_syntax_in_line(
        "= T\n\nimage::ok.png[] and image:x.png[] inline.\n",
        &["ok.png"],
        3
    )]
    #[case::inline_in_delimited_verse("= T\n\n[verse]\n____\nText image:x.png[] y\n____\n", &[], 5)]
    #[case::inline_in_verse_with_attribution(
        "= T\n\n[verse, Carl Sandburg, Fog]\nText image:x.png[] y\n",
        &[],
        4
    )]
    #[case::after_imagesdir_is_unset(
        "= T\n:imagesdir: img\n:imagesdir!:\n\nimage::x.png[]\n",
        &["img/x.png"],
        5
    )]
    #[case::file_name_with_space("= T\n\nimage::with space.png[]\n", &[], 3)]
    fn test_image_is_checked_where_asciidoctor_renders_it(
        #[case] text: &str,
        #[case] images: &[&str],
        #[case] line: usize,
    ) {
        let lines: Vec<usize> = image_messages_of(text, images)
            .into_iter()
            .map(|(line, _)| line)
            .collect();
        assert_eq!(lines, vec![line]);
    }

    /// A differing case of the extension is a missing file. Only a
    /// case-sensitive file system tells `Exists.PNG` from `exists.png`.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_image_extension_case_must_match() {
        let lines: Vec<usize> = image_messages_of("= T\n\nimage::Exists.PNG[]\n", &["exists.png"])
            .into_iter()
            .map(|(line, _)| line)
            .collect();
        assert_eq!(lines, vec![3]);
    }

    /// Contexts in which Asciidoctor renders no image or the file exists:
    /// no message.
    #[rstest::rstest]
    #[case::escaped("= T\n\nEscaped \\image:x.png[] text.\n", &[])]
    #[case::inline_passthrough("= T\n\nPass +image:x.png[]+ text.\n", &[])]
    #[case::macro_passthrough("= T\n\nMacro pass:[image:x.png[]] text.\n", &[])]
    #[case::passthrough_block("= T\n\n++++\nimage::x.png[]\n++++\n", &[])]
    #[case::line_comment("= T\n\n// image::x.png[]\n", &[])]
    #[case::block_comment("= T\n\n////\nimage::x.png[]\n////\n", &[])]
    #[case::literal_with_role("= T\n\n[literal.role]\nimage::x.png[]\n", &[])]
    #[case::source_with_options("= T\n\n[source%linenums,ruby]\nimage::x.png[]\n", &[])]
    #[case::titled_listing("= T\n\n.Titled\n[listing]\nimage::x.png[]\n", &[])]
    #[case::anchored_literal("= T\n\n[[anc]]\n[literal]\nimage::x.png[]\n", &[])]
    #[case::block_macro_in_delimited_verse("= T\n\n[verse]\n____\nimage::x.png[]\n____\n", &[])]
    #[case::nested_conditional(
        "= T\n\nifndef::nope[]\nifdef::x[]\nimage::x.png[]\nendif::[]\nendif::[]\n",
        &[]
    )]
    #[case::file_name_with_space("= T\n\nimage::with space.png[]\n", &["with space.png"])]
    fn test_no_image_diagnostic_where_asciidoctor_renders_none(
        #[case] text: &str,
        #[case] images: &[&str],
    ) {
        assert_eq!(image_messages_of(text, images), vec![]);
    }

    /// A `[verse]` paragraph is verbatim for block parsing (`image::` there
    /// stays text) but keeps the normal substitutions, inline macros
    /// included. `[listing]`, `[literal]` and `[source]` have none.
    #[test]
    fn test_inline_image_in_verse_paragraph_is_checked() {
        let source = "= T\n\n\
                      [verse]\nimage::a.png[]\n\n\
                      [verse]\nVerse image:b.png[] inline.\n\n\
                      [source]\nCode image:c.png[] inline.\n";
        let lines: Vec<usize> = image_messages_of(source, &[])
            .into_iter()
            .map(|(line, _)| line)
            .collect();
        assert_eq!(lines, vec![7]);
    }

    /// A `[verse]` paragraph whose first line is a block macro: only that
    /// line stays text, the inline image in the next line renders.
    #[test]
    fn test_inline_image_after_block_macro_line_in_verse_is_checked() {
        let source = "= T\n\n[verse]\nimage::a.png[]\nthen image:b.png[] inline.\n";
        let lines: Vec<usize> = image_messages_of(source, &[])
            .into_iter()
            .map(|(line, _)| line)
            .collect();
        assert_eq!(lines, vec![5]);
    }

    /// `part.adoc` is read whole by `book.adoc` (finds the image) and only
    /// in part by `web.adoc` (would miss it, but renders nothing of the
    /// image outside its tag), and the preview finds it too: no warning.
    #[test]
    fn test_image_not_judged_by_document_reading_file_in_part() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("img")).unwrap();
        std::fs::write(root.join("img/logo.png"), "x").unwrap();
        std::fs::write(root.join("logo.png"), "x").unwrap();
        std::fs::write(
            root.join("book.adoc"),
            "= Book\n:imagesdir: img\n\ninclude::part.adoc[]\n",
        )
        .unwrap();
        std::fs::write(
            root.join("web.adoc"),
            "= Web\n:imagesdir: nowhere\n\ninclude::part.adoc[tag=a]\n",
        )
        .unwrap();
        let part = root.join("part.adoc");
        std::fs::write(
            &part,
            "// tag::a[]\nText.\n// end::a[]\n\nimage::logo.png[]\n",
        )
        .unwrap();
        let mut ws = crate::workspace::Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();

        assert_eq!(image_messages(&ws, &part), vec![]);
    }

    /// Images the LSP cannot judge get no diagnostic: a URI target, a URI
    /// `imagesdir`, a reference to an unset attribute.
    #[rstest::rstest]
    #[case::uri_target("= T\n\nimage::https://example.com/x.png[]\n")]
    #[case::data_uri_target("= T\n\nimage::data:image/png;base64,AAAA[]\n")]
    #[case::uri_imagesdir("= T\n:imagesdir: https://cdn.example.com/img\n\nimage::x.png[]\n")]
    #[case::unresolved_attribute("= T\n\nimage::{unset-dir}/x.png[]\n")]
    fn test_image_not_judged_without_a_file_to_check(#[case] text: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let main = root.join("main.adoc");
        std::fs::write(&main, text).unwrap();
        let mut ws = crate::workspace::Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();

        assert_eq!(image_messages(&ws, &main), vec![]);
    }

    fn anchor_scope_workspace() -> (PathBuf, crate::workspace::Workspace) {
        fixture_workspace("anchor_scope")
    }

    fn broken_xrefs<'a>(
        ws: &'a crate::workspace::Workspace,
        file: &Path,
    ) -> Vec<(&'a str, Vec<String>)> {
        ws.diagnostics_for(file)
            .iter()
            .filter_map(|d| match &d.kind {
                DiagnosticKind::BrokenXref { target, documents } => Some((
                    target.as_str(),
                    documents
                        .iter()
                        .map(|doc| doc.file_name().unwrap().to_string_lossy().into_owned())
                        .collect(),
                )),
                _ => None,
            })
            .collect()
    }

    /// Asciidoctor (`web.adoc`): `INFO: possible invalid reference:
    /// setup-only`, nothing for `book.adoc`, where `setup.adoc` defines it.
    /// The shared chapter is checked per top-level document; the message
    /// names the one that lacks the target.
    #[test]
    fn test_broken_xref_in_shared_file_names_documents_where_target_is_missing() {
        let (root, ws) = anchor_scope_workspace();

        let broken = broken_xrefs(&ws, &root.join("chapters/reference.adoc"));

        assert_eq!(broken, vec![("setup-only", vec!["web.adoc".to_string()])]);
    }

    /// Asciidoctor (`book.adoc`): no message. `xref:chapters/reference.adoc#setup-only[]`
    /// is internal because `reference.adoc` is part of the document, and
    /// `_details_2` is the generated ID of the second `== Details`.
    #[test]
    fn test_xrefs_resolving_within_the_top_level_document_are_not_broken() {
        let (root, ws) = anchor_scope_workspace();

        assert_eq!(broken_xrefs(&ws, &root.join("book.adoc")), vec![]);
    }

    /// Asciidoctor (`solo/main.adoc`, `solo2/main.adoc`): no message for
    /// `<<_second>>` in either; each document has its own `_second`. This
    /// pins only that a document's own ID is no broken xref; that another
    /// document's ID is invisible is pinned by
    /// `test_xref_to_id_of_unrelated_top_level_document_is_broken`.
    #[test]
    fn test_bare_xref_to_the_documents_own_id_is_not_broken() {
        let (root, ws) = anchor_scope_workspace();

        assert_eq!(broken_xrefs(&ws, &root.join("solo/main.adoc")), vec![]);
        assert_eq!(broken_xrefs(&ws, &root.join("solo2/main.adoc")), vec![]);
    }

    /// Asciidoctor (`foo.adoc`): `INFO: possible invalid reference: _foo_3`
    /// only; the second `== Foo` is `_foo_2`.
    #[test]
    fn test_generated_id_suffix_resolves_and_the_next_does_not() {
        let (root, ws) = anchor_scope_workspace();

        assert_eq!(
            broken_xrefs(&ws, &root.join("foo.adoc")),
            vec![("_foo_3", vec![])]
        );
    }

    /// Asciidoctor does not check `file#id` into a document that is not part
    /// of the converted one (link, not reference); the LSP does, against the
    /// catalog of that file read as a document of its own, because the link
    /// would be dead in the HTML.
    #[test]
    fn test_file_xref_into_foreign_document_is_checked_against_that_file() {
        let (root, ws) = anchor_scope_workspace();

        assert_eq!(
            broken_xrefs(&ws, &root.join("solo/foreign.adoc")),
            vec![("../chapters/setup.adoc#nope", vec![])]
        );
    }

    /// Asciidoctor (`partial/book.adoc`): `xref:b.adoc#a-id[]` is internal
    /// (`b.adoc` is included in full) and fine; `xref:a.adoc#b-id[]` renders
    /// the link `a.html#b-id` because `a.adoc` is included with `lines=` and
    /// so is no part of the document. Asciidoctor does not check a link; the
    /// LSP checks it against the catalog of `a.adoc` alone, where `b-id` is
    /// missing.
    #[test]
    fn test_file_xref_into_partially_included_file_is_checked_against_that_file() {
        let (root, ws) = anchor_scope_workspace();

        assert_eq!(
            broken_xrefs(&ws, &root.join("partial/book.adoc")),
            vec![("a.adoc#b-id", vec![])]
        );
    }

    /// Asciidoctor (`anchor_selection/book.adoc`): no message, the xref
    /// renders as `href="#b-id"`. `tags=**;!t` reads all but a named tag, which
    /// is no partial include, so `a.adoc` is part of the document and `b-id`
    /// is found in `b.adoc`.
    #[test]
    fn test_file_xref_into_file_included_without_a_named_tag_is_internal() {
        let (root, ws) = fixture_workspace("anchor_selection");

        assert_eq!(broken_xrefs(&ws, &root.join("book.adoc")), vec![]);
    }

    /// Asciidoctor (`bibliography_anchors/main.adoc`): no message, `<<taoup>>`
    /// and `<<pp>>` resolve to the `[[[id]]]` entries of the bibliography.
    #[test]
    fn test_xrefs_to_bibliography_anchors_are_not_broken() {
        let (root, ws) = fixture_workspace("bibliography_anchors");

        assert_eq!(broken_xrefs(&ws, &root.join("main.adoc")), vec![]);
    }

    /// Asciidoctor (`xref_path_in_include/index.adoc`) resolves the file part
    /// of an xref in the included `chapters/setup.adoc` from the folder of
    /// `index.adoc`: `xref:chapters/reference.adoc#ref-options[]` renders as
    /// `#ref-options`, `xref:reference.adoc#ref-options[]` as the dead link
    /// `reference.html#ref-options` to the top-level `reference.adoc`, whose
    /// page has only `top-options`. `xref:setup.adoc#setup-notes[]` renders
    /// as the dead link `setup.html#setup-notes`: no `setup.adoc` lies next
    /// to `index.adoc`, and a file of that name in another folder is no
    /// target. Asciidoctor does not check these links.
    #[test]
    fn test_xref_path_in_included_file_resolves_from_the_top_level_document() {
        let (root, ws) = fixture_workspace("xref_path_in_include");

        assert_eq!(
            broken_xrefs(&ws, &root.join("chapters/setup.adoc")),
            vec![
                ("reference.adoc#ref-options", vec![]),
                ("setup.adoc#setup-notes", vec![])
            ]
        );
    }

    /// Asciidoctor (`natural_xrefs/main.adoc`): only `INFO: possible invalid
    /// reference` for `No Such Title`, `CLI usage` (the reftext hides the
    /// title), `installation` (lowercase without space is an ID only),
    /// `#Installation` (no title lookup after `#`) and `Second owner` (its
    /// reftext hides the title even though an earlier anchor holds it). On
    /// blocks: `Untitled table` (a block without ID is not matched) and
    /// `Listing title` (the block's reftext hides its title); `Table title`,
    /// `Listing text` and `Shared block title` resolve.
    #[test]
    fn test_natural_xrefs_resolve_by_title_or_reftext() {
        let (root, ws) = fixture_workspace("natural_xrefs");

        let broken: Vec<&str> = broken_xrefs(&ws, &root.join("main.adoc"))
            .into_iter()
            .map(|(target, _)| target)
            .collect();

        assert_eq!(
            broken,
            [
                "No Such Title",
                "CLI usage",
                "installation",
                "#Installation",
                "Second owner",
                "Untitled table",
                "Listing title"
            ]
        );
    }

    /// Asciidoctor (`anchor_region_xref/web3.adoc`, `reference.adoc[tag=top]`):
    /// no message, `<<setup-only>>` lies outside the tag. `book.adoc` reads
    /// the whole file and finds `setup-only`. The LSP knows no tag regions:
    /// `web3.adoc` reads only a part of the file, so it is no judge of what
    /// is missing there (prefer no diagnostic over a false one).
    #[test]
    fn test_broken_xref_outside_tag_not_reported_lsp_knows_no_tag_regions() {
        let (root, ws) = fixture_workspace("anchor_region_xref");

        let broken = broken_xrefs(&ws, &root.join("chapters/reference.adoc"));

        assert!(
            broken.iter().all(|(target, _)| *target != "setup-only"),
            "{broken:?}"
        );
    }

    /// Asciidoctor (`book.adoc`): `INFO: possible invalid reference: nowhere`;
    /// `web3.adoc` reads only the tag `top`, so it is not named. The xref
    /// inside `tag=inner` that a document reading `tag=inner` would report is
    /// a false negative, accepted.
    #[test]
    fn test_broken_xref_names_only_whole_readers() {
        let (root, ws) = fixture_workspace("anchor_region_xref");

        assert_eq!(
            broken_xrefs(&ws, &root.join("chapters/reference.adoc")),
            vec![("nowhere", vec!["book.adoc".to_string()])]
        );
    }

    /// Asciidoctor (`anchor_region_nested/web.adoc`): `INFO: possible invalid
    /// reference: nowhere`, because `snippet.adoc` lies inside `tag=x` of the
    /// wrapper. The LSP cannot tell, so a file read only below a region
    /// include gets no diagnostic (false negative, accepted).
    #[test]
    fn test_broken_xref_file_read_only_below_region_include_not_reported() {
        let (root, ws) = fixture_workspace("anchor_region_nested");

        assert_eq!(broken_xrefs(&ws, &root.join("snippet.adoc")), vec![]);
    }

    /// Asciidoctor (`anchor_single_master/master.adoc`): `INFO: possible
    /// invalid reference: nope`. With a single reading document the message
    /// names none.
    #[test]
    fn test_broken_xref_in_file_with_one_master_names_no_document() {
        let (root, ws) = fixture_workspace("anchor_single_master");

        assert_eq!(
            broken_xrefs(&ws, &root.join("part.adoc")),
            vec![("nope", vec![])]
        );
    }

    /// Asciidoctor (`a.adoc`, `b.adoc`, both including `s.adoc`): `INFO:
    /// possible invalid reference: nope` in each build. A target missing in
    /// every document that reads the file is still reported with all of
    /// them named (decision: a broken xref is reported per top-level
    /// document).
    #[test]
    fn test_broken_xref_missing_in_every_reading_document_names_all_of_them() {
        let (root, ws) = fixture_workspace("shared_missing");

        assert_eq!(
            broken_xrefs(&ws, &root.join("s.adoc")),
            vec![("nope", vec!["a.adoc".to_string(), "b.adoc".to_string()])]
        );
    }

    /// `s.adoc` is read by `a.adoc` (`edition` is `x`) and `b.adoc` (`y`).
    /// `x/doc.adoc` defines `t`, `y/doc.adoc` does not, so the file part of
    /// the xref is resolved in each document's own scope (as go-to-definition
    /// does) and only `b.adoc` lacks the target. Asciidoctor does not check
    /// such a link; the LSP does, as for any file outside the document.
    #[test]
    fn test_broken_xref_resolves_the_file_part_in_each_reading_documents_scope() {
        let (root, ws) = fixture_workspace("attr_xref");

        assert_eq!(
            broken_xrefs(&ws, &root.join("s.adoc")),
            vec![("{edition}/doc.adoc#t", vec!["b.adoc".to_string()])]
        );
    }

    /// The `DuplicateAnchor`s of `file` as (line, file name of the first
    /// definition, its line, names of the documents).
    fn duplicates(
        ws: &crate::workspace::Workspace,
        file: &Path,
    ) -> Vec<(usize, String, usize, Vec<String>)> {
        let name = |path: &Path| path.file_name().unwrap().to_string_lossy().into_owned();
        ws.diagnostics_for(file)
            .iter()
            .filter_map(|d| match &d.kind {
                DiagnosticKind::DuplicateAnchor {
                    first_file,
                    first_line,
                    documents,
                    ..
                } => Some((
                    d.line,
                    name(first_file),
                    *first_line,
                    documents.iter().map(|doc| name(doc)).collect(),
                )),
                _ => None,
            })
            .collect()
    }

    /// Asciidoctor (`book.adoc`): `WARNING: chapters/reference.adoc: line 2:
    /// id assigned to section already in use: shared`, nothing for `web.adoc`,
    /// which includes `reference.adoc` alone. The warning stands at the second
    /// definition in reading order and names the document that reads both.
    #[test]
    fn test_duplicate_id_across_included_files_is_reported_at_the_second_in_reading_order() {
        let (root, ws) = anchor_scope_workspace();

        assert_eq!(
            duplicates(&ws, &root.join("chapters/reference.adoc")),
            vec![(
                1,
                "setup.adoc".to_string(),
                1,
                vec!["book.adoc".to_string()]
            )]
        );
        assert_eq!(duplicates(&ws, &root.join("chapters/setup.adoc")), vec![]);
    }

    /// Asciidoctor (`standalone.adoc`, `web.adoc`): no warning. Documents that
    /// do not include each other may reuse an ID.
    #[test]
    fn test_same_id_in_unrelated_top_level_documents_is_no_duplicate() {
        let (root, ws) = anchor_scope_workspace();

        assert_eq!(duplicates(&ws, &root.join("standalone.adoc")), vec![]);
    }

    /// Asciidoctor (`pair/x.adoc`, `pair/y.adoc`: one warning each at
    /// `two.adoc`; `pair/z.adoc` reads `two.adoc` first: its warning is at
    /// `one.adoc`). Documents that agree on the place share one diagnostic.
    #[test]
    fn test_duplicate_id_names_every_document_that_reads_both_in_the_same_order() {
        let (root, ws) = anchor_scope_workspace();

        assert_eq!(
            duplicates(&ws, &root.join("pair/two.adoc")),
            vec![(
                1,
                "one.adoc".to_string(),
                1,
                vec!["x.adoc".to_string(), "y.adoc".to_string()]
            )]
        );
        assert_eq!(
            duplicates(&ws, &root.join("pair/one.adoc")),
            vec![(1, "two.adoc".to_string(), 1, vec!["z.adoc".to_string()])]
        );
    }

    /// Asciidoctor (`foo.adoc`): the second `== Foo` is `_foo_2`, no warning.
    #[test]
    fn test_repeated_section_title_is_no_duplicate() {
        let (root, ws) = anchor_scope_workspace();

        assert_eq!(duplicates(&ws, &root.join("foo.adoc")), vec![]);
    }

    /// Asciidoctor (`thrice.adoc`: `part.adoc` with `[[dup]]` included three
    /// times): two warnings, both at `part.adoc`'s line 1 within the one
    /// document. The diagnostic names that document once.
    #[test]
    fn test_duplicate_read_repeatedly_names_the_document_once() {
        let (root, ws) = fixture_workspace("anchor_reread");

        let found = duplicates(&ws, &root.join("part.adoc"));
        let rereads: Vec<_> = found.iter().filter(|d| d.1 == "part.adoc").collect();
        assert_eq!(
            rereads,
            vec![&(
                1,
                "part.adoc".to_string(),
                1,
                vec!["thrice.adoc".to_string()]
            )]
        );
    }

    /// Asciidoctor (`twice_after.adoc`: `first.adoc` defines `dup`, then
    /// `part.adoc` is included twice): two warnings in the one document, so
    /// the one document is named once.
    #[test]
    fn test_duplicate_pair_met_twice_after_a_first_definition_names_the_document_once() {
        let (root, ws) = fixture_workspace("anchor_reread");

        let found = duplicates(&ws, &root.join("part.adoc"));
        let after_first: Vec<_> = found.iter().filter(|d| d.1 == "first.adoc").collect();
        assert_eq!(
            after_first,
            vec![&(
                1,
                "first.adoc".to_string(),
                1,
                vec!["twice_after.adoc".to_string()]
            )]
        );
    }

    /// Asciidoctor (`implicit_then_explicit.adoc`): `== Foo` takes `_foo`,
    /// the later `[[_foo]]` warns at its line.
    #[test]
    fn test_explicit_id_after_generated_one_is_reported_at_the_explicit_id() {
        let (root, ws) = fixture_workspace("anchor_catalog");

        assert_eq!(
            duplicates(&ws, &root.join("implicit_then_explicit.adoc")),
            vec![(
                5,
                "implicit_then_explicit.adoc".to_string(),
                3,
                vec!["implicit_then_explicit.adoc".to_string()]
            )]
        );
    }

    /// Asciidoctor (`explicit_then_implicit.adoc`): `[[_foo]]` first, the
    /// generated ID of `== Foo` becomes `_foo_2`; no warning.
    #[test]
    fn test_generated_id_after_explicit_one_is_no_duplicate() {
        let (root, ws) = fixture_workspace("anchor_catalog");

        assert_eq!(
            duplicates(&ws, &root.join("explicit_then_implicit.adoc")),
            vec![]
        );
    }

    /// `a.adoc` and `b.adoc` include each other (no top-level document), `a`
    /// includes `c.adoc`. Go-to-definition reads `c.adoc` as its own document,
    /// so diagnostics check its xref there only and name no document, as
    /// Asciidoctor on `c.adoc` alone reports `INFO: possible invalid
    /// reference: only-in-a`. (On `a.adoc` Asciidoctor recurses to the
    /// include depth limit; the cycle itself is diagnosed separately.)
    #[test]
    fn test_xref_in_file_included_only_from_a_cycle_is_checked_in_its_own_document() {
        let (root, ws) = fixture_workspace("anchor_cycle");

        assert_eq!(
            broken_xrefs(&ws, &root.join("c.adoc")),
            vec![("only-in-a", vec![])]
        );
    }

    // --- Undefined attributes ---

    /// The undefined-attribute diagnostics as (file name, line, name), sorted.
    fn undefined(diags: &HashMap<PathBuf, Vec<Diagnostic>>) -> Vec<(String, usize, String)> {
        let mut found: Vec<_> = flatten(diags)
            .into_iter()
            .filter_map(|d| match &d.kind {
                DiagnosticKind::UndefinedAttribute { name, .. } => Some((
                    d.file.file_name().unwrap().to_string_lossy().into_owned(),
                    d.line,
                    name.clone(),
                )),
                _ => None,
            })
            .collect();
        found.sort();
        found
    }

    fn undefined_in_workspace(ws: &Workspace) -> Vec<(String, usize, String)> {
        undefined(&ws.diagnostics)
    }

    fn reference(name: &str, line: usize, start_col: usize) -> AttributeRef {
        AttributeRef {
            name: name.into(),
            line,
            start_col,
            end_col: start_col + name.len() + 2,
        }
    }

    fn entry(name: &str, value: &str, line: usize) -> AttributeDef {
        AttributeDef {
            name: name.into(),
            value: value.into(),
            line,
        }
    }

    /// The diagnostics of one top-level document `/a.adoc`.
    fn diagnose_one(doc: DocumentModel) -> HashMap<PathBuf, Vec<Diagnostic>> {
        let mut documents = HashMap::new();
        documents.insert(PathBuf::from("/a.adoc"), doc);
        diagnose(documents, IncludeGraph::new(), make_registry())
    }

    fn a_doc() -> DocumentModel {
        DocumentModel::test("/a.adoc")
    }

    fn lines_and_names(found: Vec<(String, usize, String)>) -> Vec<(usize, String)> {
        found
            .into_iter()
            .map(|(_, line, name)| (line, name))
            .collect()
    }

    fn open_block(line: usize) -> crate::types::UnterminatedBlock {
        crate::types::UnterminatedBlock {
            context: "listing",
            delimiter: "----".into(),
            line,
        }
    }

    #[test]
    fn test_unterminated_block_diagnostic() {
        let diags = diagnose_one(a_doc().with_unterminated_blocks(vec![open_block(7)]));
        let flat = flatten(&diags);
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].line, 7);
        assert_eq!(flat[0].span, None);
        assert_eq!(
            flat[0].kind,
            DiagnosticKind::UnterminatedBlock {
                context: "listing".into(),
                delimiter: "----".into(),
            }
        );
    }

    /// Code includes are parsed too; their lines are verbatim content of the
    /// listing around the include, not delimiters.
    #[test]
    fn test_unterminated_block_in_non_adoc_file_reports_nothing() {
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/banner.c"),
            DocumentModel::test("/banner.c").with_unterminated_blocks(vec![open_block(3)]),
        );
        let diags = diagnose(documents, IncludeGraph::new(), make_registry());
        assert!(flatten(&diags).is_empty());
    }

    #[test]
    fn undefined_attribute_is_warning_with_span() {
        let diags = diagnose_one(a_doc().with_substituted_refs(vec![reference("prodcut", 3, 6)]));
        let flat = flatten(&diags);

        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].line, 3);
        assert_eq!(
            flat[0].span,
            Some(crate::types::Span {
                start_col: 6,
                end_line: 3,
                end_col: 15,
            })
        );
        assert_eq!(
            flat[0].kind,
            DiagnosticKind::UndefinedAttribute {
                name: "prodcut".into(),
                documents: vec![],
            }
        );
    }

    /// Asciidoctor (`g-before-def`): a reference before the entry warns.
    #[test]
    fn defined_before_is_quiet_defined_after_is_reported() {
        let doc = a_doc()
            .with_attributes(vec![entry("product", "Foo", 2), entry("late", "x", 5)])
            .with_substituted_refs(vec![
                reference("product", 3, 0),
                reference("late", 3, 10),
                reference("late", 6, 0),
            ]);

        assert_eq!(
            lines_and_names(undefined(&diagnose_one(doc))),
            vec![(3, "late".to_string())]
        );
    }

    #[test]
    fn unset_entry_makes_reference_undefined() {
        let doc = a_doc()
            .with_attributes(vec![entry("product", "Foo", 2), entry("product!", "", 4)])
            .with_substituted_refs(vec![reference("product", 3, 0), reference("product", 5, 0)]);

        assert_eq!(
            lines_and_names(undefined(&diagnose_one(doc))),
            vec![(5, "product".to_string())]
        );
    }

    /// Asciidoctor: names are lower-cased on both sides.
    #[test]
    fn names_compare_case_insensitively() {
        let doc = a_doc()
            .with_attributes(vec![entry("MixedCase", "one", 1)])
            .with_substituted_refs(vec![
                reference("mixedcase", 2, 0),
                reference("MIXEDCASE", 3, 0),
            ]);

        assert_eq!(undefined(&diagnose_one(doc)), vec![]);
    }

    #[test]
    fn predefined_attributes_are_quiet() {
        let names = [
            "nbsp",
            "docname",
            "toc-title",
            "backend-pdf",
            "env-github",
            "docdir",
            "safe-mode-name",
            "doctitle",
            "empty",
            "attribute-missing",
            "attribute-undefined",
        ];
        let refs = names
            .iter()
            .enumerate()
            .map(|(index, name)| reference(name, index + 1, 0))
            .collect();

        assert_eq!(
            undefined(&diagnose_one(a_doc().with_substituted_refs(refs))),
            vec![]
        );
    }

    /// Asciidoctor warns for `{author}` without a header line, but the LSP
    /// models neither the author nor the revision line: no diagnostic is
    /// better than a false one.
    #[test]
    fn author_attributes_not_reported_lsp_has_no_header_model() {
        let names = [
            "author",
            "email",
            "firstname",
            "lastname",
            "authorinitials",
            "authors",
            "author_2",
            "email_3",
            "revnumber",
            "revdate",
            "revremark",
        ];
        let refs = names
            .iter()
            .enumerate()
            .map(|(index, name)| reference(name, index + 1, 0))
            .collect();

        assert_eq!(
            undefined(&diagnose_one(a_doc().with_substituted_refs(refs))),
            vec![]
        );
    }

    /// Asciidoctor: only `skip` and `drop` silence the warning; `drop-line`
    /// logs an INFO of its own and the LSP warns as well.
    #[rstest::rstest]
    #[case::missing(None, 1)]
    #[case::warn(Some("warn"), 1)]
    #[case::drop_line(Some("drop-line"), 1)]
    #[case::skip(Some("skip"), 0)]
    #[case::drop(Some("drop"), 0)]
    #[case::unknown(Some("bogus"), 1)]
    fn attribute_missing_modes(#[case] mode: Option<&str>, #[case] expected: usize) {
        let mut doc = a_doc().with_substituted_refs(vec![reference("x", 3, 0)]);
        if let Some(mode) = mode {
            doc = doc.with_attributes(vec![entry("attribute-missing", mode, 1)]);
        }

        assert_eq!(undefined(&diagnose_one(doc)).len(), expected);
    }

    #[test]
    fn attribute_missing_applies_from_its_position() {
        let doc = a_doc()
            .with_attributes(vec![entry("attribute-missing", "skip", 2)])
            .with_substituted_refs(vec![reference("x", 1, 0), reference("x", 3, 0)]);

        assert_eq!(
            lines_and_names(undefined(&diagnose_one(doc))),
            vec![(1, "x".to_string())]
        );
    }

    #[test]
    fn unsetting_attribute_missing_warns_again() {
        let doc = a_doc()
            .with_attributes(vec![
                entry("attribute-missing", "skip", 1),
                entry("attribute-missing!", "", 3),
            ])
            .with_substituted_refs(vec![reference("x", 2, 0), reference("x", 4, 0)]);

        assert_eq!(
            lines_and_names(undefined(&diagnose_one(doc))),
            vec![(4, "x".to_string())]
        );
    }

    #[test]
    fn attribute_missing_from_config_chain() {
        let (_, ws) = fixture_workspace("undefined_attributes_skip");

        assert_eq!(undefined_in_workspace(&ws), vec![]);
    }

    #[test]
    fn possible_definition_quiets_later_reference_only() {
        let doc = a_doc()
            .with_possible_definitions(vec![crate::types::PossibleDefinition {
                name: "x".into(),
                line: 2,
            }])
            .with_attributes(vec![entry("x!", "", 4)])
            .with_substituted_refs(vec![
                reference("x", 1, 0),
                reference("x", 3, 0),
                reference("x", 5, 0),
            ]);

        assert_eq!(
            lines_and_names(undefined(&diagnose_one(doc))),
            vec![(1, "x".to_string()), (5, "x".to_string())]
        );
    }

    /// Asciidoctor (`:a: {a}` with `a` unset before): the reference on an
    /// entry line means the value before the entry.
    #[test]
    fn reference_on_its_own_entry_line_is_reported() {
        let doc = a_doc()
            .with_attributes(vec![entry("a", "{a}", 1)])
            .with_substituted_refs(vec![reference("a", 1, 4)]);

        assert_eq!(
            lines_and_names(undefined(&diagnose_one(doc))),
            vec![(1, "a".to_string())]
        );
    }

    #[test]
    fn possible_attribute_missing_silences_later_references() {
        let doc = a_doc()
            .with_possible_definitions(vec![crate::types::PossibleDefinition {
                name: "attribute-missing".into(),
                line: 1,
            }])
            .with_substituted_refs(vec![reference("x", 2, 0)]);

        assert_eq!(undefined(&diagnose_one(doc)), vec![]);
    }

    /// An entry on a line a block conditional covers may not run, so an
    /// unset there does not make the attribute undefined.
    #[test]
    fn unset_inside_block_conditional_is_not_reported() {
        let doc = a_doc()
            .with_attributes(vec![entry("product", "Foo", 1), entry("product!", "", 3)])
            .with_possible_definitions(vec![crate::types::PossibleDefinition {
                name: "product".into(),
                line: 3,
            }])
            .with_substituted_refs(vec![reference("product", 5, 0)]);

        assert_eq!(undefined(&diagnose_one(doc)), vec![]);
    }

    #[test]
    fn file_included_twice_reports_once() {
        let mut documents = HashMap::new();
        documents.insert(PathBuf::from("/a.adoc"), a_doc());
        documents.insert(
            PathBuf::from("/b.adoc"),
            DocumentModel::test("/b.adoc").with_substituted_refs(vec![reference("y", 1, 0)]),
        );
        let mut graph = IncludeGraph::new();
        for line in [2, 4] {
            graph.add_edge(
                Path::new("/a.adoc"),
                crate::types::IncludeEdge {
                    target: PathBuf::from("/b.adoc"),
                    line,
                    attributes: vec![],
                    placement: Placement::Body,
                },
            );
        }

        let diags = diagnose(documents, graph, make_registry());

        assert_eq!(
            undefined(&diags),
            vec![("b.adoc".to_string(), 1, "y".to_string())]
        );
    }

    /// Asciidoctor on each top-level document: `book.adoc` defines the
    /// edition and is quiet, `web.adoc` warns (once per position, although
    /// it reads the file twice). `excerpt.adoc` reads only a tag of the
    /// file and is no judge: it is not named.
    #[test]
    fn shared_file_is_checked_per_top_level_document() {
        let (root, ws) = fixture_workspace("undefined_attributes_shared");

        let reported: Vec<_> = ws
            .diagnostics_for(&root.join("shared.adoc"))
            .iter()
            .filter_map(|d| match &d.kind {
                DiagnosticKind::UndefinedAttribute { name, documents } => Some((
                    d.line,
                    name.clone(),
                    documents
                        .iter()
                        .map(|doc| doc.file_name().unwrap().to_string_lossy().into_owned())
                        .collect::<Vec<_>>(),
                )),
                _ => None,
            })
            .collect();

        assert_eq!(
            reported.len(),
            2,
            "one diagnostic per position, got {reported:?}"
        );
        for (_, name, documents) in &reported {
            assert_eq!(name, "edition");
            assert_eq!(documents, &vec!["web.adoc".to_string()]);
        }
        assert_eq!(
            reported.iter().map(|(line, ..)| *line).collect::<Vec<_>>(),
            vec![3, 6]
        );
    }

    /// Asciidoctor (`index.adoc` with `-a attribute-missing=warn`): the same
    /// five warnings; everything else in the fixture stays quiet. The
    /// chapter sees `imagesdir` from the master.
    #[test]
    fn test_undefined_attributes_fixture() {
        let (_, ws) = fixture_workspace("undefined_attributes");

        assert_eq!(
            undefined_in_workspace(&ws),
            vec![
                (
                    "chapter.adoc".to_string(),
                    3,
                    "missing-in-chapter".to_string()
                ),
                ("index.adoc".to_string(), 5, "prodcut".to_string()),
                ("index.adoc".to_string(), 9, "product".to_string()),
                ("index.adoc".to_string(), 11, "host".to_string()),
                ("index.adoc".to_string(), 15, "inlisting".to_string()),
            ]
        );
    }

    /// The config defines `product` and `company` (ADR-008); the
    /// Asciidoctor CLI does not read `.asciidoctorconfig.adoc` and warns.
    #[test]
    fn test_adversarial_project_has_no_undefined_attributes() {
        let (_, ws) = fixture_workspace("adversarial_project");

        assert_eq!(undefined_in_workspace(&ws), vec![]);
    }

    // --- Comment block directly below the document title ---

    /// Asciidoctor (`attribute.adoc` with `-a attribute-missing=warn`): the
    /// comment block belongs to the header, so the entry below it defines
    /// `attr`, `{attr}` resolves and nothing warns.
    #[test]
    fn test_comment_block_below_title_keeps_the_header_attribute_defined() {
        let (root, ws) = fixture_workspace("comment_block_below_title");
        let file = root.join("attribute.adoc");

        let names: Vec<_> = ws.documents()[&file]
            .attributes
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(names, ["attr"]);
        assert_eq!(undefined_in_workspace(&ws), vec![]);
        assert!(ws.diagnostics_for(&file).is_empty());
    }

    // --- Attribute entry glued to a paragraph ---

    /// Asciidoctor (`main.adoc` with `-a attribute-missing=warn`): the
    /// `:glued: yes` line directly below a paragraph line is paragraph text
    /// (`<p>A paragraph line directly above.\n:glued: yes\n...`), so only
    /// `proper` is defined and `{glued}` warns: `skipping reference to
    /// missing attribute: glued`.
    #[test]
    fn test_entry_glued_to_a_paragraph_defines_nothing() {
        let (root, ws) = fixture_workspace("attribute_entry_glued");
        let file = root.join("main.adoc");

        let names: Vec<_> = ws.documents()[&file]
            .attributes
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(names, ["proper"]);
        assert_eq!(
            undefined_in_workspace(&ws),
            vec![("main.adoc".to_string(), 9, "glued".to_string())]
        );
    }

    // --- Section titles out of sequence (fixture `section_levels`) ---

    /// The `SectionOutOfSequence`s of `file` as (line, expected levels, got
    /// level, names of the documents).
    fn out_of_sequence(
        ws: &crate::workspace::Workspace,
        file: &Path,
    ) -> Vec<(usize, Vec<usize>, usize, Vec<String>)> {
        ws.diagnostics_for(file)
            .iter()
            .filter_map(|d| match &d.kind {
                DiagnosticKind::SectionOutOfSequence {
                    expected,
                    got,
                    documents,
                } => Some((
                    d.line,
                    expected.clone(),
                    *got,
                    documents
                        .iter()
                        .map(|doc| doc.display().to_string())
                        .collect(),
                )),
                _ => None,
            })
            .collect()
    }

    fn sequence_rows(
        ws: &crate::workspace::Workspace,
        file: &Path,
    ) -> Vec<(usize, Vec<usize>, usize)> {
        out_of_sequence(ws, file)
            .into_iter()
            .map(|(line, expected, got, _)| (line, expected, got))
            .collect()
    }

    /// Asciidoctor (`index.adoc`): `WARNING: index.adoc: line 17: section
    /// title out of sequence: expected level 3, got level 4`. The chapters
    /// included with `leveloffset=+1` raise nothing but their own skip.
    #[test]
    fn test_section_skipping_a_level_is_reported_with_asciidoctors_levels() {
        let (root, ws) = fixture_workspace("section_levels");

        assert_eq!(
            out_of_sequence(&ws, &root.join("index.adoc")),
            vec![(17, vec![3], 4, vec![])]
        );
        assert_eq!(
            ws.diagnostics_for(&root.join("index.adoc"))
                .iter()
                .find_map(
                    |d| matches!(d.kind, DiagnosticKind::SectionOutOfSequence { .. })
                        .then(|| d.span)
                ),
            Some(None)
        );
    }

    /// Asciidoctor: `WARNING: chapters/skips.adoc: line 5: ... expected level
    /// 3, got level 4`: the levels are the shifted ones, the place is the
    /// title's own file.
    #[test]
    fn test_skipped_level_in_chapter_with_leveloffset_reports_shifted_levels() {
        let (root, ws) = fixture_workspace("section_levels");

        assert_eq!(
            out_of_sequence(&ws, &root.join("chapters/skips.adoc")),
            vec![(5, vec![3], 4, vec![])]
        );
    }

    /// Asciidoctor: nothing for `chapters/clean.adoc` (a `[discrete]` title
    /// and a title in a sidebar are no sections) and nothing for
    /// `chapters/entry_offset.adoc` (`:leveloffset: +1` before the include).
    #[test]
    fn test_chapters_included_with_leveloffset_stay_quiet() {
        let (root, ws) = fixture_workspace("section_levels");

        assert_eq!(
            out_of_sequence(&ws, &root.join("chapters/clean.adoc")),
            vec![]
        );
        assert_eq!(
            out_of_sequence(&ws, &root.join("chapters/entry_offset.adoc")),
            vec![]
        );
    }

    /// Asciidoctor (`unread.adoc`), with `-a partials=shared` (`shared/
    /// unread_x.adoc` is `=== X`): only `line 11: expected level 2, got level
    /// 3`. Without the attribute it also reports line 7, next to `include
    /// file not found`. The LSP cannot resolve `{partials}`, so the include
    /// target is unread and the title after it stays unchecked.
    #[test]
    fn test_include_with_build_attribute_in_path_hides_the_parent() {
        let (root, ws) = fixture_workspace("section_levels");

        assert_eq!(
            out_of_sequence(&ws, &root.join("unread.adoc")),
            vec![(11, vec![2], 3, vec![])]
        );
    }

    /// Asciidoctor: no warning for the project, whose chapters come in with
    /// `leveloffset=+1`.
    #[test]
    fn test_adversarial_project_reports_no_section_out_of_sequence() {
        let (_, ws) = fixture_workspace("adversarial_project");

        let found: Vec<_> = ws
            .all_diagnostics()
            .into_iter()
            .filter(|d| matches!(d.kind, DiagnosticKind::SectionOutOfSequence { .. }))
            .map(|d| (d.file, d.line))
            .collect();
        assert_eq!(found, vec![]);
    }

    /// Asciidoctor (`include_attr_keys/levels.adoc`): no message. The blank
    /// after the comma does not hide `leveloffset=+1`, so `chapter.adoc`
    /// sits at level 2 and `==== After` follows it in sequence.
    #[test]
    fn test_leveloffset_after_comma_space_shifts_included_levels() {
        let (root, ws) = fixture_workspace("include_attr_keys");

        assert_eq!(out_of_sequence(&ws, &root.join("levels.adoc")), vec![]);
        assert_eq!(out_of_sequence(&ws, &root.join("chapter.adoc")), vec![]);
    }

    /// Asciidoctor (`include_attr_keys/book.adoc`): no message, the xref
    /// renders as `href="a.html#b-id"`. `tag=t` after a comma and a blank
    /// still selects a part of `a.adoc`. Asciidoctor does not check a
    /// link; the LSP checks it against the catalog of `a.adoc` alone, where
    /// `b-id` is missing.
    #[test]
    fn test_tag_after_comma_space_makes_include_partial() {
        let (root, ws) = fixture_workspace("include_attr_keys");

        assert_eq!(
            broken_xrefs(&ws, &root.join("book.adoc")),
            vec![("a.adoc#b-id", vec![])]
        );
    }

    /// Asciidoctor (`out_of_sequence/main.adoc`): `line 8: expected level 1,
    /// got level 2`.
    #[test]
    fn test_first_section_below_document_title_too_deep_is_reported() {
        let (root, ws) = fixture_workspace("out_of_sequence");

        assert_eq!(
            out_of_sequence(&ws, &root.join("main.adoc")),
            vec![(8, vec![1], 2, vec![])]
        );
    }

    /// An out-of-sequence title takes the place rule of a level 0 title.
    /// Asciidoctor (`out_of_sequence_include`): `ends_include.adoc: line 3`
    /// and `nested.adoc: line 3` (the one-line `partials/deep.adoc`, there
    /// through `partials/only_include.adoc`),
    /// `partials/deep_with_text.adoc: line 1`,
    /// `partials/include_with_text.adoc: line 1`, and for `leveloffset.adoc`
    /// `partials/deep.adoc: line 1: expected level 2, got level 3`.
    #[test]
    fn test_out_of_sequence_title_ending_an_include_is_reported_at_the_include() {
        let (root, ws) = fixture_workspace("out_of_sequence_include");
        let rows = |file: &str| sequence_rows(&ws, &root.join(file));

        assert_eq!(rows("ends_include.adoc"), vec![(3, vec![1], 2)]);
        assert_eq!(rows("nested.adoc"), vec![(3, vec![1], 2)]);
        assert_eq!(rows("partials/only_include.adoc"), vec![]);
        assert_eq!(rows("partials/deep_with_text.adoc"), vec![(1, vec![1], 2)]);
        assert_eq!(rows("title_with_text.adoc"), vec![]);
        assert_eq!(
            rows("partials/include_with_text.adoc"),
            vec![(1, vec![1], 2)]
        );
        assert_eq!(rows("include_with_text.adoc"), vec![]);
        assert_eq!(rows("partials/deep.adoc"), vec![(1, vec![2], 3)]);
        assert_eq!(rows("leveloffset.adoc"), vec![]);
    }

    /// Asciidoctor follows the content of an include with a `leveloffset`
    /// by lines of its own, so a title on the last line of the file stays
    /// there. Asciidoctor (`out_of_sequence_include/level_zero_leveloffset.adoc`):
    /// `ERROR: partials/level_one.adoc: line 1: level 0 sections can only be
    /// used when doctype is book`.
    #[test]
    fn test_level_zero_title_ending_an_include_with_leveloffset_stays_in_its_file() {
        let (root, ws) = fixture_workspace("out_of_sequence_include");

        assert_eq!(
            level_zero_outside_book(&ws, &root.join("partials/level_one.adoc")),
            vec![(1, vec![])]
        );
        assert_eq!(
            level_zero_outside_book(&ws, &root.join("level_zero_leveloffset.adoc")),
            vec![]
        );
    }

    /// Asciidoctor (`book.adoc`): `line 4: expected levels 0 or 1, got level
    /// 2`, `line 8: expected level 1, got level 2` (below the part),
    /// `line 12: expected level 2, got level 3`.
    #[test]
    fn test_book_accepts_levels_0_or_1_at_the_top_and_level_1_below_a_part() {
        let (root, ws) = fixture_workspace("section_levels");

        assert_eq!(
            sequence_rows(&ws, &root.join("book.adoc")),
            vec![(4, vec![0, 1], 2), (8, vec![1], 2), (12, vec![2], 3)]
        );
    }

    /// The `LevelZeroSectionOutsideBook`s of `file` as (line, names of the
    /// documents).
    fn level_zero_outside_book(
        ws: &crate::workspace::Workspace,
        file: &Path,
    ) -> Vec<(usize, Vec<String>)> {
        ws.diagnostics_for(file)
            .iter()
            .filter_map(|d| match &d.kind {
                DiagnosticKind::LevelZeroSectionOutsideBook { documents } => Some((
                    d.line,
                    documents
                        .iter()
                        .map(|doc| doc.display().to_string())
                        .collect(),
                )),
                _ => None,
            })
            .collect()
    }

    /// Asciidoctor (`diagnostic_paths/index.adoc`): `ERROR: chapters/one.adoc:
    /// line 1: level 0 sections can only be used when doctype is book`. The
    /// included `= One` is a section, not the header; read on its own,
    /// `chapters/one.adoc` raises no such error.
    #[test]
    fn test_level_zero_section_in_article_is_reported() {
        let (root, ws) = fixture_workspace("diagnostic_paths");

        assert_eq!(
            level_zero_outside_book(&ws, &root.join("chapters/one.adoc")),
            vec![(1, vec![])]
        );
        assert_eq!(
            level_zero_outside_book(&ws, &root.join("index.adoc")),
            vec![]
        );
    }

    /// Asciidoctor (`book.adoc`, `:doctype: book`): only the three
    /// out-of-sequence warnings, nothing for `= Part` on line 6.
    /// Asciidoctor reports a title on the last line of an included file at
    /// the include, as it has left the file by then, and so on outward while
    /// that include is the last line of its file. Asciidoctor
    /// (`docdir_top_level/docs/handbook/handbook.adoc`, which includes
    /// `../shared/chapters.adoc`, whose only line includes the one-line
    /// `chapters/intro.adoc`): `ERROR: handbook.adoc: line 5: level 0 sections
    /// can only be used when doctype is book`.
    #[test]
    fn test_level_zero_title_ending_an_include_is_reported_at_the_include() {
        let (root, ws) = fixture_workspace("docdir_top_level");
        let handbook = root.join("docs/handbook");

        assert_eq!(
            level_zero_outside_book(&ws, &handbook.join("handbook.adoc")),
            vec![(5, vec![])]
        );
        for file in ["chapters/intro.adoc", "../shared/chapters.adoc"] {
            assert_eq!(level_zero_outside_book(&ws, &handbook.join(file)), vec![]);
        }
    }

    /// Asciidoctor (`symlinks/real/project/book/twice.adoc`, which includes
    /// the one-line `../part-up.adoc` at line 3): `ERROR: twice.adoc: line 3:
    /// level 0 sections can only be used when doctype is book`.
    #[test]
    fn test_level_zero_title_ending_an_include_above_the_document_is_reported_at_the_include() {
        let (root, ws) = fixture_workspace("symlinks/real/project");

        assert!(
            level_zero_outside_book(&ws, &root.join("book/twice.adoc"))
                .iter()
                .any(|(line, _)| *line == 3),
            "{:?}",
            ws.diagnostics_for(&root.join("book/twice.adoc"))
        );
    }

    #[test]
    fn test_level_zero_section_in_book_is_not_reported() {
        let (root, ws) = fixture_workspace("section_levels");

        assert_eq!(
            level_zero_outside_book(&ws, &root.join("book.adoc")),
            vec![]
        );
    }

    /// Asciidoctor (`level_zero_book_config/index.adoc`): `ERROR:
    /// chapters/one.adoc: line 1: level 0 sections can only be used when
    /// doctype is book` without attributes, nothing with `-a doctype=book`
    /// (the CLI does not read `.asciidoctorconfig`, which sets it here).
    #[test]
    fn test_level_zero_section_with_doctype_book_from_config_is_not_reported() {
        let (root, ws) = fixture_workspace("level_zero_book_config");

        assert_eq!(
            level_zero_outside_book(&ws, &root.join("chapters/one.adoc")),
            vec![]
        );
    }

    /// Asciidoctor (`headerless_level_zero.adoc`): no message. Without a
    /// document header the level 0 title after a first section is accepted.
    #[test]
    fn test_level_zero_section_in_headerless_document_is_not_reported() {
        let (root, ws) = fixture_workspace("section_levels");

        assert_eq!(
            level_zero_outside_book(&ws, &root.join("headerless_level_zero.adoc")),
            vec![]
        );
    }

    /// Asciidoctor (`special.adoc`): an `ERROR` for the nested section in the
    /// glossary (not ours) and a `WARNING` for line 11 only, below the
    /// appendix. The glossary's children are not checked.
    #[test]
    fn test_special_section_children_are_not_checked_appendix_children_are() {
        let (root, ws) = fixture_workspace("section_levels");

        assert_eq!(
            sequence_rows(&ws, &root.join("special.adoc")),
            vec![(11, vec![2], 3)]
        );
    }

    /// Asciidoctor (`headerless.adoc`): `line 5: expected level 2, got level
    /// 3`; the opening `===` is not checked.
    #[test]
    fn test_headerless_document_leaves_its_top_sections_unchecked() {
        let (root, ws) = fixture_workspace("section_levels");

        assert_eq!(
            sequence_rows(&ws, &root.join("headerless.adoc")),
            vec![(5, vec![2], 3)]
        );
    }

    /// Asciidoctor (`headerless_preamble.adoc`): `line 3: expected level 1,
    /// got level 2`, because the preamble paragraph comes first. The LSP knows
    /// no block before the first title: a deliberate false negative, which
    /// keeps orphaned partials (`=== X` at the top) quiet.
    #[test]
    fn test_headerless_section_after_preamble_not_reported_lsp_does_not_see_the_preamble() {
        let (root, ws) = fixture_workspace("section_levels");

        assert_eq!(
            sequence_rows(&ws, &root.join("headerless_preamble.adoc")),
            vec![]
        );
    }

    /// Asciidoctor: `WARNING: shared/part.adoc: line 1: expected level 2, got
    /// level 3` when `print.adoc` includes it with `leveloffset=+1`, nothing
    /// for `web.adoc`. The message names the document.
    #[test]
    fn test_shared_file_out_of_sequence_in_one_document_names_it() {
        let (root, ws) = fixture_workspace("section_levels");
        let part = root.join("shared/part.adoc");

        assert_eq!(
            out_of_sequence(&ws, &part),
            vec![(1, vec![2], 3, vec!["print.adoc".to_string()])]
        );
        assert_eq!(
            ws.diagnostics_for(&part)[0].kind.to_string(),
            "Section title out of sequence: expected level 2, got level 3 (in print.adoc)"
        );
        assert_eq!(out_of_sequence(&ws, &root.join("web.adoc")), vec![]);
    }

    /// Asciidoctor (`region.adoc`): only `line 11` (after `== Back`).
    /// What `tag=t` reads is unknown to the LSP, so line 7 (`====` after the
    /// region) is not judged against `== Before`, and `regions/tagged.adoc`
    /// reports nothing.
    #[test]
    fn test_region_include_hides_the_parent_until_a_shallower_section() {
        let (root, ws) = fixture_workspace("section_levels");

        assert_eq!(
            sequence_rows(&ws, &root.join("region.adoc")),
            vec![(11, vec![2], 3)]
        );
        assert_eq!(
            out_of_sequence(&ws, &root.join("regions/tagged.adoc")),
            vec![]
        );
    }

    /// Asciidoctor: `shared/sets_offset.adoc` line 4 (`:leveloffset: +1` in
    /// the file on top of the include's own `+1`), nothing in `restore.adoc`:
    /// after the include the value of before it is back.
    #[test]
    fn test_leveloffset_of_an_include_is_restored_after_it() {
        let (root, ws) = fixture_workspace("section_levels");

        assert_eq!(
            out_of_sequence(&ws, &root.join("shared/sets_offset.adoc")),
            vec![(4, vec![2], 3, vec![])]
        );
        assert_eq!(out_of_sequence(&ws, &root.join("restore.adoc")), vec![]);
    }

    /// Asciidoctor (`offsets.adoc`): `line 5: expected level 1, got level 2`
    /// (absolute `:leveloffset: 1`), `line 11: expected level 2, got level 3`
    /// (after `:leveloffset!:`), and `shared/absolute.adoc: line 5: expected
    /// level 3, got level 4` (`leveloffset=1` on the include).
    #[test]
    fn test_absolute_and_unset_leveloffset_entries() {
        let (root, ws) = fixture_workspace("section_levels");

        assert_eq!(
            sequence_rows(&ws, &root.join("offsets.adoc")),
            vec![(5, vec![1], 2), (11, vec![2], 3)]
        );
        assert_eq!(
            sequence_rows(&ws, &root.join("shared/absolute.adoc")),
            vec![(5, vec![3], 4)]
        );
    }

    /// Asciidoctor: no warning; an include inside a listing or an example
    /// block is block content, not sections.
    #[test]
    fn test_includes_inside_blocks_contribute_no_sections() {
        let (root, ws) = fixture_workspace("section_levels");

        assert_eq!(out_of_sequence(&ws, &root.join("listing.adoc")), vec![]);
        assert_eq!(
            out_of_sequence(&ws, &root.join("shared/deep_sample.adoc")),
            vec![]
        );
    }

    /// A file read twice by one document reports its skip once.
    #[test]
    fn test_file_read_twice_in_one_document_reports_once() {
        let top = |line| IncludeMacro {
            path: "b.adoc".into(),
            attributes: vec![],
            line,
            target_col: 0,
            placement: Placement::Body,
        };
        let section = |level, line| Section {
            level,
            title: "T".into(),
            line,
            id: Some("id".into()),
            is_header: level == 0,
            style: None,
            placement: Placement::Body,
        };
        let mut documents = HashMap::new();
        documents.insert(
            PathBuf::from("/a.adoc"),
            DocumentModel::test("/a.adoc")
                .with_sections(vec![section(0, 1), section(1, 3)])
                .with_includes(vec![top(5), top(7)]),
        );
        documents.insert(
            PathBuf::from("/b.adoc"),
            DocumentModel::test("/b.adoc").with_sections(vec![section(3, 1)]),
        );
        let mut graph = IncludeGraph::new();
        for line in [5, 7] {
            graph.add_edge(
                Path::new("/a.adoc"),
                crate::types::IncludeEdge {
                    target: PathBuf::from("/b.adoc"),
                    line,
                    attributes: vec![],
                    placement: Placement::Body,
                },
            );
        }

        let diags = diagnose(documents, graph, make_registry());

        let found: Vec<_> = diags[&PathBuf::from("/b.adoc")]
            .iter()
            .map(|d| (d.line, d.kind.to_string()))
            .collect();
        assert_eq!(
            found,
            vec![(
                1,
                "Section title out of sequence: expected level 2, got level 3".to_string()
            )]
        );
    }

    /// A file read twice by one document and once by another names each
    /// document once.
    #[test]
    fn test_out_of_sequence_names_a_document_reading_the_file_twice_once() {
        let section = |level, line| Section {
            level,
            title: "T".into(),
            line,
            id: Some("id".into()),
            is_header: level == 0,
            style: None,
            placement: Placement::Body,
        };
        let top = |line| IncludeMacro {
            path: "b.adoc".into(),
            attributes: vec![],
            line,
            target_col: 0,
            placement: Placement::Body,
        };
        let mut documents = HashMap::new();
        let mut graph = IncludeGraph::new();
        for (name, lines) in [("/a.adoc", vec![5, 7]), ("/c.adoc", vec![5])] {
            documents.insert(
                PathBuf::from(name),
                DocumentModel::test(name)
                    .with_sections(vec![section(0, 1), section(1, 3)])
                    .with_includes(lines.iter().map(|&line| top(line)).collect()),
            );
            for line in lines {
                graph.add_edge(
                    Path::new(name),
                    crate::types::IncludeEdge {
                        target: PathBuf::from("/b.adoc"),
                        line,
                        attributes: vec![],
                        placement: Placement::Body,
                    },
                );
            }
        }
        documents.insert(
            PathBuf::from("/b.adoc"),
            DocumentModel::test("/b.adoc").with_sections(vec![section(3, 1)]),
        );

        let diags = diagnose(documents, graph, make_registry());

        let found: Vec<_> = diags[&PathBuf::from("/b.adoc")]
            .iter()
            .map(|d| d.kind.to_string())
            .collect();
        assert_eq!(
            found,
            vec![
                "Section title out of sequence: expected level 2, got level 3 (in /a.adoc, /c.adoc)"
                    .to_string()
            ]
        );
    }

    fn attrs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// Asciidoctor include selection, oracle-checked per case (see the
    /// `include_tags` fixture): `lines` > `tag` > `tags`, the first present
    /// key decides even when empty; `tag` is one name, `tags` splits at `;`
    /// and `,`; negations and wildcards are never looked up.
    #[rstest::rstest]
    #[case::tag(&[("tag", "missing")], &["missing"])]
    #[case::tag_is_not_split(&[("tag", "\"a;b\"")], &["a;b"])]
    #[case::tags_semicolon(&[("tags", "\"m1;m2\"")], &["m1", "m2"])]
    #[case::tags_unquoted_comma_ends_value(&[("tags", "m1,m2")], &["m1"])]
    #[case::tags_deduplicated(&[("tags", "\"m;m\"")], &["m"])]
    #[case::tags_entries_not_trimmed(&[("tags", "\"a; b\"")], &["a", " b"])]
    #[case::tag_quoted_space(&[("tag", "\" a\"")], &[" a"])]
    #[case::single_quotes(&[("tag", "'a'")], &["a"])]
    #[case::edge_whitespace(&[("tag", " a ")], &["a"])]
    #[case::negated_tags(&[("tags", "\"!m\"")], &[])]
    #[case::negated_tag(&[("tag", "!m")], &[])]
    #[case::wildcards_with_negation(&[("tags", "\"**;!m\"")], &[])]
    #[case::star_tags(&[("tags", "\"*\"")], &[])]
    #[case::double_star_tags(&[("tags", "\"**\"")], &[])]
    #[case::star_tag(&[("tag", "*")], &[])]
    #[case::double_star_tag(&[("tag", "**")], &[])]
    #[case::only_separators(&[("tags", "\";;\"")], &[])]
    #[case::only_bang_tags(&[("tags", "\"!\"")], &[])]
    #[case::only_bang_tag(&[("tag", "!")], &[])]
    #[case::empty_tag(&[("tag", "")], &[])]
    #[case::no_key(&[("leveloffset", "1")], &[])]
    #[case::star_then_name(&[("tags", "\"*;m\"")], &["m"])]
    #[case::negated_wildcard_then_name(&[("tags", "\"!**;m\"")], &["m"])]
    #[case::name_then_negation(&[("tags", "\"m;!n\"")], &["m"])]
    #[case::lines_wins(&[("lines", "1..2"), ("tag", "m")], &[])]
    #[case::lines_wins_in_any_order(&[("tag", "m"), ("lines", "1")], &[])]
    #[case::empty_lines_wins(&[("lines", ""), ("tag", "m")], &[])]
    #[case::tag_wins_over_tags(&[("tag", "m"), ("tags", "p")], &["m"])]
    #[case::tag_wins_in_any_order(&[("tags", "m"), ("tag", "p")], &["p"])]
    #[case::empty_tag_wins_over_tags(&[("tag", ""), ("tags", "m")], &[])]
    #[case::duplicate_tag_last_wins(&[("tag", "missing"), ("tag", "present")], &["present"])]
    #[case::duplicate_tag_last_wins_second(&[("tag", "present"), ("tag", "missing")], &["missing"])]
    #[case::unterminated_double_quote_no_diagnostic(&[("tags", "\"present")], &[])]
    #[case::unterminated_single_quote_no_diagnostic(&[("tags", "'a")], &[])]
    #[case::unquoted_tags_comma_ends_value(&[("tags", "present,m30")], &["present"])]
    fn test_required_tags_follows_asciidoctor_selection(
        #[case] attributes: &[(&str, &str)],
        #[case] expected: &[&str],
    ) {
        assert_eq!(required_tags(&attrs(attributes), str::to_string), expected);
    }

    #[test]
    fn test_required_tags_substitutes_attributes() {
        let substitute = |value: &str| value.replace("{x}", "p");
        assert_eq!(required_tags(&attrs(&[("tag", "{x}")]), substitute), ["p"]);
    }

    /// Asciidoctor reports `tag '{undefined}' not found`; the LSP cannot know
    /// build-time attributes, so it stays quiet rather than risk a false one.
    #[test]
    fn test_required_tags_unresolved_attribute_no_diagnostic_lsp_lacks_build_attributes() {
        assert_eq!(
            required_tags(&attrs(&[("tag", "{x}")]), str::to_string),
            Vec::<String>::new()
        );
    }

    fn include_tag_diagnostics(
        ws: &crate::workspace::Workspace,
        file: &Path,
    ) -> Vec<(usize, String)> {
        let mut found: Vec<_> = ws
            .diagnostics_for(file)
            .iter()
            .filter(|d| matches!(d.kind, DiagnosticKind::IncludeTagNotFound { .. }))
            .map(|d| (d.line, d.kind.to_string()))
            .collect();
        found.sort();
        found
    }

    fn missing_tag(line: usize, names: &str, path: &str) -> (usize, String) {
        let word = if names.contains(',') { "tags" } else { "tag" };
        (
            line,
            format!("{word} '{names}' not found in include file: {path}"),
        )
    }

    const CONFIG: &str = "snippets/config.yml";

    /// Asciidoctor 2.0.26, `asciidoctor -v -o - tests/fixtures/include_tags/main.adoc`:
    /// the `tag '...' not found` warnings by line. Left out: line 25,
    /// `tag={undefined}`, where the LSP does not know build-time attributes.
    #[test]
    fn test_include_tags_match_asciidoctor() {
        let (root, ws) = fixture_workspace("include_tags");

        let expected = vec![
            missing_tag(4, "missing", CONFIG),
            missing_tag(6, "m1, m2", CONFIG),
            missing_tag(7, "m3", CONFIG),
            missing_tag(8, "m4", CONFIG),
            missing_tag(9, "m18", CONFIG),
            missing_tag(10, "m16", CONFIG),
            missing_tag(15, "m8", CONFIG),
            missing_tag(17, "present;m10", CONFIG),
            missing_tag(18, " m15", CONFIG),
            missing_tag(20, "mq", CONFIG),
            missing_tag(22, "m11", CONFIG),
            missing_tag(26, "glued", CONFIG),
            missing_tag(27, "onlyend", CONFIG),
            missing_tag(28, "case", CONFIG),
            missing_tag(32, "absent", "part.adoc"),
            missing_tag(36, "listing", CONFIG),
            missing_tag(39, "m14", CONFIG),
            missing_tag(40, "m19", CONFIG),
            missing_tag(52, "missing2", CONFIG),
            missing_tag(53, "m50", CONFIG),
        ];
        assert_eq!(
            include_tag_diagnostics(&ws, &root.join("main.adoc")),
            expected
        );
    }

    #[test]
    fn test_include_tag_missing_is_reported_on_the_include_line() {
        let (root, ws) = fixture_workspace("include_tags");

        assert!(include_tag_diagnostics(&ws, &root.join("main.adoc"))
            .contains(&missing_tag(4, "missing", CONFIG)));
    }

    #[test]
    fn test_include_tag_present_is_quiet() {
        let (root, ws) = fixture_workspace("include_tags");

        let lines: Vec<_> = include_tag_diagnostics(&ws, &root.join("main.adoc"))
            .into_iter()
            .map(|(line, _)| line)
            .collect();
        // tag=present, tag=*, tag='present', tags=m12,tag=present, tag={tagname},
        // tag=incond, part.adoc[tag=section], tag=,tags=m22, tag=missing1,tag=present,
        // tags=m53, tag=present, tag=m36, lines=1, tags=,lines=1
        for quiet in [5, 14, 19, 23, 24, 29, 31, 50, 51, 54, 55, 56] {
            assert!(!lines.contains(&quiet), "line {quiet} must be quiet");
        }
    }

    #[test]
    fn test_include_tags_missing_are_reported_together() {
        let (root, ws) = fixture_workspace("include_tags");

        assert!(include_tag_diagnostics(&ws, &root.join("main.adoc"))
            .contains(&missing_tag(6, "m1, m2", CONFIG)));
    }

    #[test]
    fn test_include_tag_inside_listing_block_is_checked() {
        let (root, ws) = fixture_workspace("include_tags");

        assert!(include_tag_diagnostics(&ws, &root.join("main.adoc"))
            .contains(&missing_tag(36, "listing", CONFIG)));
    }

    #[test]
    fn test_include_tag_missing_in_adoc_target_is_reported() {
        let (root, ws) = fixture_workspace("include_tags");

        assert!(
            include_tag_diagnostics(&ws, &root.join("main.adoc")).contains(&missing_tag(
                32,
                "absent",
                "part.adoc"
            ))
        );
    }

    #[test]
    fn test_include_tag_on_missing_file_reports_only_broken_include() {
        let (root, ws) = fixture_workspace("include_tags");

        let on_line: Vec<_> = ws
            .diagnostics_for(&root.join("main.adoc"))
            .iter()
            .filter(|d| d.line == 30)
            .map(|d| &d.kind)
            .collect();
        assert!(matches!(
            on_line.as_slice(),
            [DiagnosticKind::BrokenInclude { .. }]
        ));
    }

    #[test]
    fn test_include_tag_quoted_comma_value_is_read_whole() {
        let (root, ws) = fixture_workspace("include_tags");

        assert!(include_tag_diagnostics(&ws, &root.join("main.adoc"))
            .contains(&missing_tag(39, "m14", CONFIG)));
    }

    /// A non-adoc target is always read from disk, even when the workspace
    /// holds a (possibly stale) model of it.
    #[test]
    fn test_include_tag_non_adoc_target_is_read_from_disk_not_from_model() {
        let (root, mut ws) = fixture_workspace("include_tags");
        let config = root.join("snippets/config.yml");
        let buffer = "# tag::present[]\n# tag::missing[]\n";
        let overrides = HashMap::from([(config.clone(), std::sync::Arc::new(buffer.to_string()))]);

        ws.update_file(&config, &overrides);
        ws.recalculate_diagnostics();

        assert!(include_tag_diagnostics(&ws, &root.join("main.adoc"))
            .contains(&missing_tag(4, "missing", CONFIG)));
    }

    #[test]
    fn test_include_tag_added_in_open_buffer_is_found() {
        let (root, mut ws) = fixture_workspace("include_tags");
        let part = root.join("part.adoc");
        let buffer = "= Part\n\n// tag::section[]\nText.\n// end::section[]\n// tag::absent[]\n// end::absent[]\n";
        let overrides = HashMap::from([(part.clone(), std::sync::Arc::new(buffer.to_string()))]);

        ws.update_file(&part, &overrides);
        ws.recalculate_diagnostics();

        assert!(
            !include_tag_diagnostics(&ws, &root.join("main.adoc")).contains(&missing_tag(
                32,
                "absent",
                "part.adoc"
            ))
        );
    }

    /// Asciidoctor 2.0.26, `asciidoctor -v -o - tests/fixtures/optional_include/main.adoc`:
    /// INFO "optional include dropped" on lines 10-16, ERROR "include file
    /// not found" on lines 18-21; the present optional file reports nothing.
    #[test]
    fn test_optional_include_not_found_matches_asciidoctor() {
        let (root, ws) = fixture_workspace("optional_include");

        let mut found: Vec<_> = ws
            .diagnostics_for(&root.join("main.adoc"))
            .iter()
            .filter_map(|d| match &d.kind {
                DiagnosticKind::OptionalIncludeNotFound { path } => {
                    Some((d.line, "info", path.clone()))
                }
                DiagnosticKind::BrokenInclude { path, .. } => Some((d.line, "error", path.clone())),
                _ => None,
            })
            .collect();
        found.sort();

        let expected: Vec<_> = [
            (10, "info", "missing.adoc"),
            (11, "info", "list-first.adoc"),
            (12, "info", "list-last.adoc"),
            (13, "info", "from-attribute.adoc"),
            (14, "info", "repeated.adoc"),
            (15, "info", "option-attribute.adoc"),
            (16, "info", "quoted.adoc"),
            (18, "error", "plain.adoc"),
            (19, "error", "other-option.adoc"),
            (20, "error", "shorthand.adoc"),
            (21, "error", "uppercase.adoc"),
        ]
        .into_iter()
        .map(|(line, level, path)| (line, level, path.to_string()))
        .collect();
        assert_eq!(found, expected);
    }

    /// The include diagnostics of `file` as (level, path, documents named).
    fn include_diagnostics(
        ws: &Workspace,
        file: &Path,
    ) -> Vec<(&'static str, String, Vec<String>)> {
        ws.diagnostics_for(file)
            .iter()
            .filter_map(|d| match &d.kind {
                DiagnosticKind::BrokenInclude { path, documents } => Some((
                    "error",
                    path.clone(),
                    documents
                        .iter()
                        .map(|doc| doc.display().to_string())
                        .collect(),
                )),
                DiagnosticKind::OptionalIncludeNotFound { path } => {
                    Some(("info", path.clone(), Vec::new()))
                }
                _ => None,
            })
            .collect()
    }

    /// A scanned workspace of `files` (relative path, content) in a
    /// temporary folder, with its canonical root.
    fn written_workspace(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, Workspace) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for (path, content) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        ws.recalculate_diagnostics();
        (tmp, root, ws)
    }

    /// A broken include names the documents that lack the target, counted
    /// as xrefs are: among every document that reads the file, also one
    /// that reads it only in part and so never counts as missing.
    #[test]
    fn test_broken_include_names_its_documents_although_a_partial_reader_is_no_judge() {
        let (_tmp, root, ws) = written_workspace(&[
            ("book.adoc", "= Book\n\ninclude::part.adoc[]\n"),
            ("web.adoc", "= Web\n\ninclude::part.adoc[tag=a]\n"),
            (
                "part.adoc",
                "// tag::a[]\ninclude::gone.adoc[]\n// end::a[]\n",
            ),
        ]);

        assert_eq!(
            include_diagnostics(&ws, &root.join("part.adoc")),
            vec![(
                "error",
                "gone.adoc".to_string(),
                vec!["book.adoc".to_string()]
            )]
        );
    }

    /// Each top-level document is judged with its own attributes; the
    /// diagnostic names those where the target is missing.
    #[test]
    fn test_broken_include_names_the_documents_where_it_is_missing() {
        let (_tmp, root, ws) = written_workspace(&[
            ("book.adoc", "= Book\n:d: a\n\ninclude::part.adoc[]\n"),
            ("web.adoc", "= Web\n:d: b\n\ninclude::part.adoc[]\n"),
            ("part.adoc", "include::{d}/x.adoc[]\n"),
            ("a/x.adoc", "X\n"),
        ]);

        assert_eq!(
            include_diagnostics(&ws, &root.join("part.adoc")),
            vec![(
                "error",
                "{d}/x.adoc".to_string(),
                vec!["web.adoc".to_string()]
            )]
        );
    }

    /// Only documents that read a file in part read it: what is missing in
    /// it is no one's to report.
    #[test]
    fn test_include_missing_in_a_file_only_partial_readers_read_is_not_reported() {
        let (_tmp, root, ws) = written_workspace(&[
            ("web.adoc", "= Web\n\ninclude::part.adoc[tag=a]\n"),
            (
                "part.adoc",
                "// tag::a[]\nText.\n// end::a[]\n\ninclude::gone.adoc[]\n",
            ),
        ]);

        assert_eq!(include_diagnostics(&ws, &root.join("part.adoc")), vec![]);
    }
}
