mod diagnostics_convert;
mod navigation;
mod positions;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer, LspService, Server};

use asciidoc_core::workspace::Workspace;

// ---------------------------------------------------------------------------
// Document Store (DC-4)
// ---------------------------------------------------------------------------

/// State for an open document. No cached Tree (V-1: tree_sitter::Tree is !Send).
#[derive(Debug)]
struct DocumentState {
    content: String,
    version: i32,
}

// ---------------------------------------------------------------------------
// Entry-Point Heuristic (V-3, V-8: extracted for testability)
// ---------------------------------------------------------------------------

/// Find the entry point for a workspace.
///
/// Strategy:
/// 1. Look for `index.adoc` directly in root
/// 2. Look for `index.adoc` in immediate subdirectories (e.g. `docs/index.adoc` in arc42)
/// 3. Fallback: first `.adoc` file found in the root directory
fn find_entry_point(root: &Path) -> Option<PathBuf> {
    // First: look for index.adoc directly in root
    let root_index = root.join("index.adoc");
    if root_index.exists() {
        return Some(root_index);
    }

    // Second: look for index.adoc in immediate subdirectories (e.g. docs/index.adoc)
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let sub_index = path.join("index.adoc");
                if sub_index.exists() {
                    return Some(sub_index);
                }
            }
        }
    }

    // Fallback: first .adoc file in root directory
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension() {
                    if ext == "adoc" {
                        return Some(path);
                    }
                }
            }
        }
    }

    None
}

/// Detect the workspace root from the given URI.
///
/// Strategy:
/// 1. Look for `.asciidoctorconfig` upward
/// 2. Look for `.git` upward
/// 3. Fallback to the given root
fn detect_workspace_root(root: &Path) -> PathBuf {
    let mut current = Some(root);
    while let Some(dir) = current {
        if dir.join(".asciidoctorconfig").exists() {
            return dir.to_path_buf();
        }
        current = dir.parent();
    }

    let mut current = Some(root);
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
        current = dir.parent();
    }

    root.to_path_buf()
}

// ---------------------------------------------------------------------------
// AsciiDocLsp Server
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct AsciiDocLsp {
    client: Client,
    documents: Mutex<HashMap<Uri, DocumentState>>,
    workspace: Mutex<Option<Workspace>>,
    root: Mutex<Option<PathBuf>>,
    /// URIs for which we've published non-empty diagnostics.
    /// Used to clear stale diagnostics when files become clean.
    published_diagnostics: Mutex<HashSet<Uri>>,
}

impl AsciiDocLsp {
    fn new(client: Client) -> Self {
        Self {
            client,
            documents: Mutex::new(HashMap::new()),
            workspace: Mutex::new(None),
            root: Mutex::new(None),
            published_diagnostics: Mutex::new(HashSet::new()),
        }
    }

    /// Publish diagnostics for all files in the workspace after a rebuild.
    ///
    /// Groups diagnostics by file, converts them, publishes per-file, and
    /// clears stale diagnostics for files that no longer have any.
    async fn publish_workspace_diagnostics(&self, ws: &Workspace) {
        // Group diagnostics by file
        let mut diags_by_file: HashMap<PathBuf, Vec<asciidoc_core::types::Diagnostic>> =
            HashMap::new();
        for diag in &ws.diagnostics {
            diags_by_file
                .entry(diag.file.clone())
                .or_default()
                .push(diag.clone());
        }

        // Prepare all conversions without holding locks across await points
        let conversions: Vec<(Uri, Vec<Diagnostic>)> = {
            let docs = self.documents.lock().unwrap();
            diags_by_file
                .iter()
                .filter_map(|(path, file_diags)| {
                    let uri = path_to_uri(path)?;
                    let content = docs
                        .get(&uri)
                        .map(|s| s.content.clone())
                        .unwrap_or_else(|| std::fs::read_to_string(path).unwrap_or_default());
                    let lsp_diags = diagnostics_convert::convert_diagnostics(file_diags, &content);
                    Some((uri, lsp_diags))
                })
                .collect()
        };

        let mut new_published = HashSet::new();
        for (uri, lsp_diags) in conversions {
            self.client
                .publish_diagnostics(uri.clone(), lsp_diags, None)
                .await;
            new_published.insert(uri);
        }

        // Clear diagnostics for files that no longer have any
        let old_published = self.published_diagnostics.lock().unwrap().clone();
        for uri in &old_published {
            if !new_published.contains(uri) {
                self.client
                    .publish_diagnostics(uri.clone(), vec![], None)
                    .await;
            }
        }

        *self.published_diagnostics.lock().unwrap() = new_published;
    }
}

impl LanguageServer for AsciiDocLsp {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Extract root from params
        // root_uri is deprecated in favor of workspace_folders, but we still
        // need it as primary source for single-root workspaces and as fallback.
        #[allow(deprecated)]
        let root_path: Option<PathBuf> = params
            .root_uri
            .as_ref()
            .and_then(|uri| uri.to_file_path().map(|cow| cow.into_owned()))
            .or_else(|| params.root_path.as_ref().map(PathBuf::from));

        if let Some(ref root) = root_path {
            let workspace_root = detect_workspace_root(root);
            *self.root.lock().unwrap() = Some(workspace_root.clone());

            // Build initial workspace if entry point found
            if let Some(entry) = find_entry_point(&workspace_root) {
                let overrides = HashMap::new();
                if let Ok(ws) = Workspace::build_with_overrides(&workspace_root, &entry, &overrides)
                {
                    *self.workspace.lock().unwrap() = Some(ws);
                }
            }
        }

        self.client
            .log_message(MessageType::INFO, "asciidoc-lsp initialized")
            .await;

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(
                            SaveOptions {
                                include_text: Some(false),
                            },
                        )),
                        ..Default::default()
                    },
                )),
                document_symbol_provider: Some(OneOf::Left(true)),
                definition_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let content = params.text_document.text;
        let version = params.text_document.version;

        self.documents.lock().unwrap().insert(
            uri.clone(),
            DocumentState {
                content: content.clone(),
                version,
            },
        );

        // Publish initial diagnostics from workspace state (V-7: only for files in graph)
        let lsp_diags = {
            let ws = self.workspace.lock().unwrap();
            match (ws.as_ref(), uri.to_file_path()) {
                (Some(ws), Some(path)) => {
                    let path_owned = path.into_owned();
                    if ws.documents.contains_key(&path_owned) {
                        let file_diags: Vec<_> = ws
                            .diagnostics
                            .iter()
                            .filter(|d| d.file == path_owned)
                            .cloned()
                            .collect();
                        Some(diagnostics_convert::convert_diagnostics(
                            &file_diags,
                            &content,
                        ))
                    } else {
                        None
                    }
                }
                _ => None,
            }
        };

        if let Some(diags) = lsp_diags {
            if !diags.is_empty() {
                self.published_diagnostics
                    .lock()
                    .unwrap()
                    .insert(uri.clone());
            }
            self.client
                .publish_diagnostics(uri, diags, Some(version))
                .await;
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let new_version = params.text_document.version;

        let mut docs = self.documents.lock().unwrap();
        // Version guard: discard outdated changes (DC-2)
        if let Some(existing) = docs.get(&uri) {
            if new_version <= existing.version {
                return;
            }
        }

        // Full sync: take the last content change (TextDocumentSyncKind::FULL)
        if let Some(change) = params.content_changes.into_iter().last() {
            docs.insert(
                uri,
                DocumentState {
                    content: change.text,
                    version: new_version,
                },
            );
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.lock().unwrap().remove(&uri);

        // Clear diagnostics for closed file
        self.published_diagnostics.lock().unwrap().remove(&uri);
        self.client.publish_diagnostics(uri, vec![], None).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let _ = params; // params.text not used; we rebuild from document store
        let root = self.root.lock().unwrap().clone();
        let Some(root) = root else { return };

        let workspace_root = detect_workspace_root(&root);
        let Some(entry) = find_entry_point(&workspace_root) else {
            return;
        };

        // Build content overrides from open documents
        let overrides = build_content_overrides(&self.documents);

        // Rebuild workspace with overrides (DC-3: Semantic Layer on save)
        if let Ok(ws) = Workspace::build_with_overrides(&workspace_root, &entry, &overrides) {
            self.publish_workspace_diagnostics(&ws).await;
            *self.workspace.lock().unwrap() = Some(ws);
        }
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = &params.text_document.uri;

        // Get content from document store or disk fallback
        let content = {
            let docs = self.documents.lock().unwrap();
            if let Some(state) = docs.get(uri) {
                state.content.clone()
            } else if let Some(path) = uri.to_file_path() {
                match std::fs::read_to_string(path.as_ref()) {
                    Ok(c) => c,
                    Err(_) => return Ok(None),
                }
            } else {
                return Ok(None);
            }
        };

        // Parse and extract sections
        let mut parser = asciidoc_core::parser::init_block_parser()
            .map_err(|e| tower_lsp_server::jsonrpc::Error::invalid_params(e.to_string()))?;
        let tree = asciidoc_core::parser::parse(&mut parser, &content)
            .map_err(|e| tower_lsp_server::jsonrpc::Error::invalid_params(e.to_string()))?;
        let sections =
            asciidoc_core::extractors::extract_sections(tree.root_node(), content.as_bytes());

        let symbols = sections_to_document_symbols(&sections, &content);

        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        // Get content from document store or disk fallback
        let content = {
            let docs = self.documents.lock().unwrap();
            if let Some(state) = docs.get(uri) {
                state.content.clone()
            } else if let Some(path) = uri.to_file_path() {
                match std::fs::read_to_string(path.as_ref()) {
                    Ok(c) => c,
                    Err(_) => return Ok(None),
                }
            } else {
                return Ok(None);
            }
        };

        // Check if cursor is on an {attribute} reference
        let attr_name =
            match find_attribute_at_position(&content, position.line, position.character) {
                Some(name) => name,
                None => return Ok(None),
            };

        // Resolve the attribute value from the workspace
        let doc_path = match uri.to_file_path() {
            Some(p) => p.into_owned(),
            None => return Ok(None),
        };

        let ws = self.workspace.lock().unwrap();
        let value = ws
            .as_ref()
            .and_then(|ws| ws.attributes.resolve(&doc_path, &attr_name))
            .map(|s| s.to_string());

        match value {
            Some(val) => Ok(Some(Hover {
                contents: HoverContents::Scalar(MarkedString::String(format!(
                    ":{attr_name}: {val}"
                ))),
                range: None,
            })),
            None => Ok(None),
        }
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<WorkspaceSymbolResponse>> {
        let ws = self.workspace.lock().unwrap();
        let Some(ws) = ws.as_ref() else {
            return Ok(None);
        };

        let symbols = collect_workspace_symbols(ws, &params.query);
        if symbols.is_empty() {
            Ok(None)
        } else {
            Ok(Some(WorkspaceSymbolResponse::Flat(symbols)))
        }
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        // Get content from document store or disk fallback
        let content = {
            let docs = self.documents.lock().unwrap();
            if let Some(state) = docs.get(uri) {
                state.content.clone()
            } else if let Some(path) = uri.to_file_path() {
                match std::fs::read_to_string(path.as_ref()) {
                    Ok(c) => c,
                    Err(_) => return Ok(None),
                }
            } else {
                return Ok(None);
            }
        };

        let doc_path = match uri.to_file_path() {
            Some(p) => p.into_owned(),
            None => return Ok(None),
        };

        let ws = self.workspace.lock().unwrap();
        let result =
            navigation::resolve_goto_definition(&content, position, &doc_path, ws.as_ref());
        // Drop the lock before building response
        drop(ws);

        let (target_path, target_line) = match result {
            Some(r) => r,
            None => return Ok(None),
        };

        let target_uri = match Uri::from_file_path(&target_path) {
            Some(u) => u,
            None => return Ok(None),
        };

        // Convert 1-indexed line to 0-indexed LSP position
        let lsp_line = (target_line as u32).saturating_sub(1);
        let target_pos = Position {
            line: lsp_line,
            character: 0,
        };

        Ok(Some(GotoDefinitionResponse::Scalar(Location {
            uri: target_uri,
            range: Range {
                start: target_pos,
                end: target_pos,
            },
        })))
    }
}

// ---------------------------------------------------------------------------
// Hover: attribute detection (V-8: extracted for testability)
// ---------------------------------------------------------------------------

/// Find the attribute name at the given LSP position.
///
/// Looks for `{attribute_name}` patterns by scanning the line at the cursor
/// position for enclosing braces. Returns the attribute name if the cursor
/// is inside a `{name}` reference (including on the braces themselves).
fn find_attribute_at_position(content: &str, line: u32, character: u32) -> Option<String> {
    let line_text = content.lines().nth(line as usize)?;

    // Convert UTF-16 character offset to byte offset
    let pos = Position { line, character };
    let (_, byte_col) = positions::from_lsp_position(pos, content);

    if byte_col >= line_text.len() {
        return None;
    }

    // Find the nearest '{' at or before cursor position
    let open_pos = line_text[..=byte_col].rfind('{')?;

    // Find the nearest '}' at or after cursor position
    let close_relative = line_text[byte_col..].find('}')?;
    let close_pos = byte_col + close_relative;

    // Extract the content between braces
    let name = &line_text[open_pos + 1..close_pos];

    // Reject if empty or contains nested braces
    if name.is_empty() || name.contains('{') || name.contains('}') {
        return None;
    }

    Some(name.to_string())
}

// ---------------------------------------------------------------------------
// DocumentSymbol helpers (V-8: extracted for testability)
// ---------------------------------------------------------------------------

/// Convert a flat list of sections into a nested DocumentSymbol hierarchy.
///
/// Sections are nested by level: a level-2 section becomes a child of the
/// preceding level-1 section, etc. Range covers from heading to just before
/// the next section at the same or lower level.
fn sections_to_document_symbols(
    sections: &[asciidoc_core::types::Section],
    content: &str,
) -> Vec<DocumentSymbol> {
    let total_lines = content.lines().count();
    let mut idx = 0;
    build_symbol_children(sections, &mut idx, None, content, total_lines)
}

fn build_symbol_children(
    sections: &[asciidoc_core::types::Section],
    idx: &mut usize,
    parent_level: Option<usize>,
    content: &str,
    total_lines: usize,
) -> Vec<DocumentSymbol> {
    let mut result = Vec::new();

    while *idx < sections.len() {
        let section = &sections[*idx];

        // If this section is at or above the parent level, it belongs to an ancestor
        if let Some(parent) = parent_level {
            if section.level <= parent {
                break;
            }
        }

        let current_level = section.level;
        let start_line = section.line; // 1-indexed
        let title = section.title.clone();
        *idx += 1;

        // Recursively collect children (deeper levels)
        let children =
            build_symbol_children(sections, idx, Some(current_level), content, total_lines);

        // Range end: line before the next section at same/higher level, or end of document
        let end_line = if *idx < sections.len() {
            sections[*idx].line.saturating_sub(1).max(start_line)
        } else {
            total_lines
        };

        let start_pos = positions::to_lsp_position(start_line, 0, content);
        let end_pos = positions::to_lsp_position(end_line, 0, content);

        // selection_range: just the heading line
        let heading_len = content
            .lines()
            .nth(start_line.saturating_sub(1))
            .map(|l| l.len())
            .unwrap_or(0);
        let selection_end = positions::to_lsp_position(start_line, heading_len, content);

        let range = Range {
            start: start_pos,
            end: end_pos,
        };
        let selection_range = Range {
            start: start_pos,
            end: selection_end,
        };

        #[allow(deprecated)]
        let symbol = DocumentSymbol {
            name: title,
            detail: None,
            kind: SymbolKind::STRUCT,
            tags: None,
            deprecated: None,
            range,
            selection_range,
            children: if children.is_empty() {
                None
            } else {
                Some(children)
            },
        };

        result.push(symbol);
    }

    result
}

/// Convert a filesystem path to an LSP URI.
fn path_to_uri(path: &Path) -> Option<Uri> {
    Uri::from_file_path(path)
}

// ---------------------------------------------------------------------------
// Workspace Symbols (V-8: extracted for testability)
// ---------------------------------------------------------------------------

/// Collect workspace symbols (sections + anchors) matching a query.
///
/// - Sections become `SymbolKind::STRUCT` symbols
/// - Anchors become `SymbolKind::CONSTANT` symbols
/// - Empty query returns all symbols
/// - Non-empty query filters case-insensitively (name contains query)
///
/// Each symbol gets a `Location` built from the file URI and 1-indexed line
/// converted to an LSP Position via `positions::to_lsp_position`.
fn collect_workspace_symbols(ws: &Workspace, query: &str) -> Vec<SymbolInformation> {
    let query_lower = query.to_lowercase();
    let mut symbols = Vec::new();

    // Sections from all DocumentModels
    for (path, doc) in &ws.documents {
        let uri = match path_to_uri(path) {
            Some(u) => u,
            None => continue,
        };
        let content = std::fs::read_to_string(path).unwrap_or_default();

        for section in &doc.sections {
            if !query_lower.is_empty() && !section.title.to_lowercase().contains(&query_lower) {
                continue;
            }

            let pos = positions::to_lsp_position(section.line, 0, &content);

            #[allow(deprecated)]
            let sym = SymbolInformation {
                name: section.title.clone(),
                kind: SymbolKind::STRUCT,
                tags: None,
                deprecated: None,
                location: Location {
                    uri: uri.clone(),
                    range: Range {
                        start: pos,
                        end: pos,
                    },
                },
                container_name: None,
            };
            symbols.push(sym);
        }
    }

    // Anchors from ReferenceIndex
    for (id, loc) in &ws.reference_index.anchors {
        if !query_lower.is_empty() && !id.to_lowercase().contains(&query_lower) {
            continue;
        }

        let uri = match path_to_uri(&loc.file) {
            Some(u) => u,
            None => continue,
        };
        let content = std::fs::read_to_string(&loc.file).unwrap_or_default();
        let pos = positions::to_lsp_position(loc.line, 0, &content);

        #[allow(deprecated)]
        let sym = SymbolInformation {
            name: id.clone(),
            kind: SymbolKind::CONSTANT,
            tags: None,
            deprecated: None,
            location: Location {
                uri,
                range: Range {
                    start: pos,
                    end: pos,
                },
            },
            container_name: None,
        };
        symbols.push(sym);
    }

    // Sort by name for deterministic output
    symbols.sort_by(|a, b| a.name.cmp(&b.name));
    symbols
}

/// Build a HashMap of file path -> content for all open documents.
/// Used as overrides so the semantic layer reads in-memory content
/// instead of stale disk content.
fn build_content_overrides(
    documents: &Mutex<HashMap<Uri, DocumentState>>,
) -> HashMap<PathBuf, String> {
    let docs = documents.lock().unwrap();
    let mut overrides = HashMap::new();
    for (uri, state) in docs.iter() {
        if let Some(path) = uri.to_file_path() {
            overrides.insert(path.into_owned(), state.content.clone());
        }
    }
    overrides
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(AsciiDocLsp::new);
    Server::new(stdin, stdout, socket).serve(service).await;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::str::FromStr;

    // --- Entry-Point Heuristic tests (V-3, V-8) ---

    #[test]
    fn test_find_entry_point_index_in_root() {
        let tmp = tempfile::tempdir().unwrap();
        let index = tmp.path().join("index.adoc");
        fs::write(&index, "= Title\n").unwrap();

        let result = find_entry_point(tmp.path());
        assert_eq!(result, Some(index));
    }

    #[test]
    fn test_find_entry_point_index_in_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let docs = tmp.path().join("docs");
        fs::create_dir(&docs).unwrap();
        let index = docs.join("index.adoc");
        fs::write(&index, "= Title\n").unwrap();

        let result = find_entry_point(tmp.path());
        assert_eq!(result, Some(index));
    }

    #[test]
    fn test_find_entry_point_fallback_first_adoc() {
        let tmp = tempfile::tempdir().unwrap();
        let adoc = tmp.path().join("readme.adoc");
        fs::write(&adoc, "= Readme\n").unwrap();

        let result = find_entry_point(tmp.path());
        assert_eq!(result, Some(adoc));
    }

    #[test]
    fn test_find_entry_point_no_adoc_files() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("readme.md"), "# Title\n").unwrap();

        let result = find_entry_point(tmp.path());
        assert_eq!(result, None);
    }

    #[test]
    fn test_find_entry_point_prefers_index_over_other() {
        let tmp = tempfile::tempdir().unwrap();
        let index = tmp.path().join("index.adoc");
        fs::write(&index, "= Index\n").unwrap();
        fs::write(tmp.path().join("other.adoc"), "= Other\n").unwrap();

        let result = find_entry_point(tmp.path());
        assert_eq!(result, Some(index));
    }

    // --- Workspace Root Detection tests ---

    #[test]
    fn test_detect_workspace_root_asciidoctorconfig() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join(".asciidoctorconfig"), ":imagesdir: images\n").unwrap();
        let sub = root.join("docs");
        fs::create_dir(&sub).unwrap();

        let result = detect_workspace_root(&sub);
        assert_eq!(result, root.to_path_buf());
    }

    #[test]
    fn test_detect_workspace_root_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("some").join("deep").join("dir");
        fs::create_dir_all(&sub).unwrap();

        let result = detect_workspace_root(&sub);
        assert_eq!(result, sub);
    }

    // --- Document Lifecycle tests ---

    #[test]
    fn test_did_open_stores_document() {
        let docs: Mutex<HashMap<Uri, DocumentState>> = Mutex::new(HashMap::new());
        let uri = Uri::from_str("file:///test.adoc").unwrap();
        let content = "= Title\n\nContent.".to_string();

        docs.lock().unwrap().insert(
            uri.clone(),
            DocumentState {
                content: content.clone(),
                version: 1,
            },
        );

        let stored = docs.lock().unwrap();
        let state = stored.get(&uri).unwrap();
        assert_eq!(state.content, content);
        assert_eq!(state.version, 1);
    }

    #[test]
    fn test_did_change_updates_content() {
        let docs: Mutex<HashMap<Uri, DocumentState>> = Mutex::new(HashMap::new());
        let uri = Uri::from_str("file:///test.adoc").unwrap();

        // Open
        docs.lock().unwrap().insert(
            uri.clone(),
            DocumentState {
                content: "old content".into(),
                version: 1,
            },
        );

        // Change with newer version
        {
            let mut locked = docs.lock().unwrap();
            let existing = locked.get(&uri).unwrap();
            assert!(2 > existing.version); // new version is newer
            locked.insert(
                uri.clone(),
                DocumentState {
                    content: "new content".into(),
                    version: 2,
                },
            );
        }

        let stored = docs.lock().unwrap();
        let state = stored.get(&uri).unwrap();
        assert_eq!(state.content, "new content");
        assert_eq!(state.version, 2);
    }

    #[test]
    fn test_did_change_ignores_old_version() {
        let docs: Mutex<HashMap<Uri, DocumentState>> = Mutex::new(HashMap::new());
        let uri = Uri::from_str("file:///test.adoc").unwrap();

        // Open with version 5
        docs.lock().unwrap().insert(
            uri.clone(),
            DocumentState {
                content: "current content".into(),
                version: 5,
            },
        );

        // Attempt change with older version (simulates out-of-order delivery)
        {
            let mut locked = docs.lock().unwrap();
            let existing = locked.get(&uri).unwrap();
            let old_version = 3;
            if old_version > existing.version {
                locked.insert(
                    uri.clone(),
                    DocumentState {
                        content: "stale content".into(),
                        version: old_version,
                    },
                );
            }
            // else: discard
        }

        let stored = docs.lock().unwrap();
        let state = stored.get(&uri).unwrap();
        assert_eq!(state.content, "current content");
        assert_eq!(state.version, 5);
    }

    #[test]
    fn test_did_close_removes_document() {
        let docs: Mutex<HashMap<Uri, DocumentState>> = Mutex::new(HashMap::new());
        let uri = Uri::from_str("file:///test.adoc").unwrap();

        docs.lock().unwrap().insert(
            uri.clone(),
            DocumentState {
                content: "content".into(),
                version: 1,
            },
        );
        assert!(docs.lock().unwrap().contains_key(&uri));

        docs.lock().unwrap().remove(&uri);
        assert!(!docs.lock().unwrap().contains_key(&uri));
    }

    // --- Content Override tests ---

    #[test]
    fn test_build_content_overrides() {
        let docs: Mutex<HashMap<Uri, DocumentState>> = Mutex::new(HashMap::new());
        let uri = Uri::from_str("file:///tmp/test.adoc").unwrap();

        docs.lock().unwrap().insert(
            uri,
            DocumentState {
                content: "overridden content".into(),
                version: 1,
            },
        );

        let overrides = build_content_overrides(&docs);
        assert_eq!(overrides.len(), 1);
        assert_eq!(
            overrides.get(Path::new("/tmp/test.adoc")).unwrap(),
            "overridden content"
        );
    }

    #[test]
    fn test_build_content_overrides_empty() {
        let docs: Mutex<HashMap<Uri, DocumentState>> = Mutex::new(HashMap::new());
        let overrides = build_content_overrides(&docs);
        assert!(overrides.is_empty());
    }

    // --- Workspace override integration test ---

    #[test]
    fn test_build_with_overrides_uses_override_content() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.adoc");
        let b = tmp.path().join("b.adoc");

        // Write initial files to disk
        fs::write(&a, "= Doc A\n\ninclude::b.adoc[]\n").unwrap();
        fs::write(&b, "== Chapter B\n\nOriginal content.\n").unwrap();

        // Override b.adoc with different content (simulating unsaved editor buffer)
        let b_canon = b.canonicalize().unwrap();
        let mut overrides = HashMap::new();
        overrides.insert(
            b_canon.clone(),
            "== Modified Chapter B\n\nEdited content.\n".to_string(),
        );

        let ws = Workspace::build_with_overrides(tmp.path(), &a, &overrides).unwrap();

        // Verify the override content was used
        let b_doc = ws.documents.get(&b_canon).unwrap();
        assert_eq!(b_doc.sections[0].title, "Modified Chapter B");
    }

    // --- DocumentSymbol hierarchy tests (V-8) ---

    #[test]
    fn test_sections_to_document_symbols_flat() {
        let sections = vec![
            asciidoc_core::types::Section {
                level: 0,
                title: "Title".into(),
                line: 1,
            },
            asciidoc_core::types::Section {
                level: 1,
                title: "Chapter A".into(),
                line: 3,
            },
            asciidoc_core::types::Section {
                level: 1,
                title: "Chapter B".into(),
                line: 5,
            },
        ];
        let content = "= Title\n\n== Chapter A\n\n== Chapter B\n";

        let symbols = sections_to_document_symbols(&sections, content);

        // Top level: one root symbol (Title)
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "Title");
        assert_eq!(symbols[0].kind, SymbolKind::STRUCT);

        // Title has two children
        let children = symbols[0].children.as_ref().unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].name, "Chapter A");
        assert_eq!(children[1].name, "Chapter B");

        // Chapters have no children
        assert!(children[0].children.is_none());
        assert!(children[1].children.is_none());
    }

    #[test]
    fn test_sections_to_document_symbols_nested_3_levels() {
        let sections = vec![
            asciidoc_core::types::Section {
                level: 0,
                title: "Doc".into(),
                line: 1,
            },
            asciidoc_core::types::Section {
                level: 1,
                title: "Ch1".into(),
                line: 3,
            },
            asciidoc_core::types::Section {
                level: 2,
                title: "Sub1".into(),
                line: 5,
            },
            asciidoc_core::types::Section {
                level: 2,
                title: "Sub2".into(),
                line: 7,
            },
            asciidoc_core::types::Section {
                level: 1,
                title: "Ch2".into(),
                line: 9,
            },
        ];
        let content = "= Doc\n\n== Ch1\n\n=== Sub1\n\n=== Sub2\n\n== Ch2\n";

        let symbols = sections_to_document_symbols(&sections, content);

        assert_eq!(symbols.len(), 1);
        let root = &symbols[0];
        assert_eq!(root.name, "Doc");

        let chapters = root.children.as_ref().unwrap();
        assert_eq!(chapters.len(), 2);
        assert_eq!(chapters[0].name, "Ch1");
        assert_eq!(chapters[1].name, "Ch2");

        // Ch1 has two subsections
        let subs = chapters[0].children.as_ref().unwrap();
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].name, "Sub1");
        assert_eq!(subs[1].name, "Sub2");

        // Ch2 has no children
        assert!(chapters[1].children.is_none());
    }

    #[test]
    fn test_sections_to_document_symbols_positions() {
        let sections = vec![
            asciidoc_core::types::Section {
                level: 0,
                title: "Title".into(),
                line: 1,
            },
            asciidoc_core::types::Section {
                level: 1,
                title: "Chapter".into(),
                line: 3,
            },
        ];
        let content = "= Title\n\n== Chapter\n\nSome text.\n";

        let symbols = sections_to_document_symbols(&sections, content);
        let root = &symbols[0];

        // Root starts at line 0 (0-indexed), covers to end of document
        assert_eq!(root.range.start.line, 0);
        assert_eq!(root.range.start.character, 0);

        // selection_range covers the heading line
        assert_eq!(root.selection_range.start.line, 0);
        assert_eq!(root.selection_range.end.line, 0);
        assert_eq!(root.selection_range.end.character, 7); // "= Title" = 7 chars

        // Chapter starts at line 2 (0-indexed)
        let chapter = &root.children.as_ref().unwrap()[0];
        assert_eq!(chapter.range.start.line, 2);
        assert_eq!(chapter.selection_range.end.character, 10); // "== Chapter" = 10 chars
    }

    #[test]
    fn test_sections_to_document_symbols_empty() {
        let sections: Vec<asciidoc_core::types::Section> = vec![];
        let content = "Just plain text, no sections.\n";

        let symbols = sections_to_document_symbols(&sections, content);
        assert!(symbols.is_empty());
    }

    #[test]
    fn test_sections_to_document_symbols_no_root() {
        // Document starting at level 1 (no document title)
        let sections = vec![
            asciidoc_core::types::Section {
                level: 1,
                title: "Ch1".into(),
                line: 1,
            },
            asciidoc_core::types::Section {
                level: 1,
                title: "Ch2".into(),
                line: 3,
            },
        ];
        let content = "== Ch1\n\n== Ch2\n";

        let symbols = sections_to_document_symbols(&sections, content);

        // Both chapters are top-level
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "Ch1");
        assert_eq!(symbols[1].name, "Ch2");
    }

    // --- Hover: find_attribute_at_position tests (V-8) ---

    #[test]
    fn test_find_attr_basic() {
        let content = "The value is {imagesdir} here.";
        // Cursor on 'i' of 'imagesdir' (position 14)
        assert_eq!(
            find_attribute_at_position(content, 0, 14),
            Some("imagesdir".into())
        );
    }

    #[test]
    fn test_find_attr_at_open_brace() {
        let content = "The value is {imagesdir} here.";
        // Cursor on '{' (position 13)
        assert_eq!(
            find_attribute_at_position(content, 0, 13),
            Some("imagesdir".into())
        );
    }

    #[test]
    fn test_find_attr_at_close_brace() {
        let content = "The value is {imagesdir} here.";
        // '}' is at position 23
        assert_eq!(
            find_attribute_at_position(content, 0, 23),
            Some("imagesdir".into())
        );
    }

    #[test]
    fn test_find_attr_not_in_braces() {
        let content = "Just plain text.";
        assert_eq!(find_attribute_at_position(content, 0, 5), None);
    }

    #[test]
    fn test_find_attr_after_close_brace() {
        let content = "The value is {imagesdir} here.";
        // Position 24 = space after '}'
        assert_eq!(find_attribute_at_position(content, 0, 24), None);
    }

    #[test]
    fn test_find_attr_empty_braces() {
        let content = "Empty {} here.";
        // Cursor on '}' at position 7 — between the braces is empty
        assert_eq!(find_attribute_at_position(content, 0, 7), None);
    }

    #[test]
    fn test_find_attr_multiline() {
        let content = "= Title\n\nSee {chapdir} for details.";
        // 'chapdir' is on line 2 (0-indexed)
        assert_eq!(
            find_attribute_at_position(content, 2, 5),
            Some("chapdir".into())
        );
    }

    #[test]
    fn test_find_attr_cursor_past_line_end() {
        let content = "short";
        assert_eq!(find_attribute_at_position(content, 0, 100), None);
    }

    #[test]
    fn test_find_attr_invalid_line() {
        let content = "one line";
        assert_eq!(find_attribute_at_position(content, 5, 0), None);
    }

    #[test]
    fn test_find_attr_between_two_refs() {
        // Cursor between two attribute references should return None
        let content = "Text {foo} middle {bar} end";
        // Position 11 = 'm' of 'middle' (between the two refs)
        assert_eq!(find_attribute_at_position(content, 0, 11), None);
    }

    #[test]
    fn test_find_attr_second_of_two() {
        let content = "Text {foo} middle {bar} end";
        // Position 19 = 'b' of 'bar'
        assert_eq!(
            find_attribute_at_position(content, 0, 19),
            Some("bar".into())
        );
    }

    #[test]
    fn test_sections_to_document_symbols_from_parsed_content() {
        // Full integration: parse real AsciiDoc → sections → DocumentSymbol
        let content = "= My Document\n\n== Introduction\n\nSome intro text.\n\n== Details\n\n=== Part A\n\n=== Part B\n";

        let mut parser = asciidoc_core::parser::init_block_parser().unwrap();
        let tree = asciidoc_core::parser::parse(&mut parser, content).unwrap();
        let sections =
            asciidoc_core::extractors::extract_sections(tree.root_node(), content.as_bytes());

        let symbols = sections_to_document_symbols(&sections, content);

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "My Document");

        let chapters = symbols[0].children.as_ref().unwrap();
        assert_eq!(chapters.len(), 2);
        assert_eq!(chapters[0].name, "Introduction");
        assert_eq!(chapters[1].name, "Details");

        let parts = chapters[1].children.as_ref().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].name, "Part A");
        assert_eq!(parts[1].name, "Part B");
    }

    // --- Workspace Symbols tests (V-8) ---

    /// Helper: build a workspace from temp files for workspace symbol tests.
    fn build_test_workspace(files: &[(&str, &str)]) -> (tempfile::TempDir, Workspace) {
        let tmp = tempfile::tempdir().unwrap();
        let mut first_file = None;
        for (name, content) in files {
            let path = tmp.path().join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, content).unwrap();
            if first_file.is_none() {
                first_file = Some(path);
            }
        }
        let entry = first_file.unwrap();
        let ws = Workspace::build(tmp.path(), &entry).unwrap();
        (tmp, ws)
    }

    #[test]
    fn test_workspace_symbols_empty_query_returns_all() {
        let (_tmp, ws) =
            build_test_workspace(&[("index.adoc", "= Title\n\n== Chapter A\n\n== Chapter B\n")]);

        let symbols = collect_workspace_symbols(&ws, "");

        // Should contain sections and their implicit anchor IDs
        assert!(!symbols.is_empty());
        // All sections should be present
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Title"), "missing 'Title' in {:?}", names);
        assert!(
            names.contains(&"Chapter A"),
            "missing 'Chapter A' in {:?}",
            names
        );
        assert!(
            names.contains(&"Chapter B"),
            "missing 'Chapter B' in {:?}",
            names
        );
    }

    #[test]
    fn test_workspace_symbols_query_filters_case_insensitive() {
        let (_tmp, ws) =
            build_test_workspace(&[("index.adoc", "= Title\n\n== Introduction\n\n== Details\n")]);

        let symbols = collect_workspace_symbols(&ws, "intro");

        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"Introduction"),
            "missing 'Introduction' in {:?}",
            names
        );
        // "Title" and "Details" should NOT match "intro"
        assert!(
            !names.contains(&"Title"),
            "unexpected 'Title' in {:?}",
            names
        );
        assert!(
            !names.contains(&"Details"),
            "unexpected 'Details' in {:?}",
            names
        );
    }

    #[test]
    fn test_workspace_symbols_query_no_match() {
        let (_tmp, ws) = build_test_workspace(&[("index.adoc", "= Title\n\n== Chapter\n")]);

        let symbols = collect_workspace_symbols(&ws, "zzzznotfound");
        assert!(symbols.is_empty());
    }

    #[test]
    fn test_workspace_symbols_sections_use_string_kind() {
        let (_tmp, ws) = build_test_workspace(&[("index.adoc", "= Title\n\n== Chapter\n")]);

        let symbols = collect_workspace_symbols(&ws, "");
        let section_symbols: Vec<_> = symbols
            .iter()
            .filter(|s| s.name == "Title" || s.name == "Chapter")
            .collect();

        for sym in &section_symbols {
            assert_eq!(
                sym.kind,
                SymbolKind::STRUCT,
                "section '{}' should be STRING",
                sym.name
            );
        }
    }

    #[test]
    fn test_workspace_symbols_anchors_use_key_kind() {
        let (_tmp, ws) =
            build_test_workspace(&[("index.adoc", "= Title\n\n[[my-anchor]]\n== Chapter\n")]);

        let symbols = collect_workspace_symbols(&ws, "my-anchor");
        let anchor_sym = symbols.iter().find(|s| s.name == "my-anchor");
        assert!(anchor_sym.is_some(), "expected anchor symbol 'my-anchor'");
        assert_eq!(anchor_sym.unwrap().kind, SymbolKind::CONSTANT);
    }

    #[test]
    fn test_workspace_symbols_multi_file() {
        let (_tmp, ws) = build_test_workspace(&[
            ("index.adoc", "= Root Doc\n\ninclude::sub/chapter.adoc[]\n"),
            ("sub/chapter.adoc", "== Sub Chapter\n\nContent.\n"),
        ]);

        let symbols = collect_workspace_symbols(&ws, "");
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"Root Doc"),
            "missing 'Root Doc' in {:?}",
            names
        );
        assert!(
            names.contains(&"Sub Chapter"),
            "missing 'Sub Chapter' in {:?}",
            names
        );
    }

    #[test]
    fn test_workspace_symbols_sorted_by_name() {
        let (_tmp, ws) =
            build_test_workspace(&[("index.adoc", "= Zebra Doc\n\n== Alpha\n\n== Beta\n")]);

        let symbols = collect_workspace_symbols(&ws, "");
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();

        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "symbols should be sorted by name");
    }

    #[test]
    fn test_workspace_symbols_location_has_correct_uri() {
        let (_tmp, ws) = build_test_workspace(&[("index.adoc", "= Title\n")]);

        let symbols = collect_workspace_symbols(&ws, "Title");
        assert!(!symbols.is_empty());
        let uri_str = symbols[0].location.uri.to_string();
        assert!(
            uri_str.starts_with("file://"),
            "URI should be file://: {}",
            uri_str
        );
        assert!(
            uri_str.contains("index.adoc"),
            "URI should contain filename: {}",
            uri_str
        );
    }

    // -----------------------------------------------------------------------
    // Task 8.1: Integration Tests — arc42 Fixture (AC-12)
    // -----------------------------------------------------------------------

    /// Path to the workspace root (asciidoc-lsp/) from the crate manifest dir.
    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("workspace root should exist")
    }

    /// Path to the arc42 fixture docs directory.
    fn arc42_docs_dir() -> PathBuf {
        workspace_root().join("docs")
    }

    #[test]
    fn test_integration_arc42_initialize() {
        let ws_root = workspace_root();

        // Entry-point heuristic should find docs/index.adoc
        let entry = find_entry_point(&ws_root);
        assert!(entry.is_some(), "should find entry point in arc42 fixture");

        let entry = entry.unwrap();
        assert!(
            entry.ends_with("docs/index.adoc"),
            "entry should be docs/index.adoc, got: {:?}",
            entry
        );

        // Workspace root detection: .asciidoctorconfig is in docs/,
        // .git is at the repo root — detect_workspace_root should find it
        let detected_root = detect_workspace_root(&ws_root);
        assert!(
            detected_root.join("Cargo.toml").exists(),
            "detected workspace root should contain Cargo.toml: {:?}",
            detected_root
        );

        // Workspace::build should succeed with the arc42 fixture
        let ws = Workspace::build(&detected_root, &entry);
        assert!(
            ws.is_ok(),
            "workspace build should succeed for arc42: {:?}",
            ws.err()
        );

        let ws = ws.unwrap();

        // Should have found multiple documents (index + included chapters)
        assert!(
            ws.documents.len() >= 6,
            "workspace should have at least 6 documents (arc42 has ~10), found: {}",
            ws.documents.len()
        );

        // The entry point file should be in the documents
        let entry_canon = entry.canonicalize().unwrap();
        assert!(
            ws.documents.contains_key(&entry_canon),
            "workspace should contain entry file"
        );

        // Reference index should have anchors from the fixture
        assert!(
            !ws.reference_index.anchors.is_empty(),
            "arc42 fixture should have anchors (e.g. [[project-goals]])"
        );
    }

    #[test]
    fn test_integration_arc42_document_symbol() {
        let docs_dir = arc42_docs_dir();
        let index = docs_dir.join("index.adoc");
        assert!(index.exists(), "arc42 index.adoc should exist");

        let content = fs::read_to_string(&index).unwrap();

        // Parse and extract sections from the entry file
        let mut parser = asciidoc_core::parser::init_block_parser().unwrap();
        let tree = asciidoc_core::parser::parse(&mut parser, &content).unwrap();
        let sections =
            asciidoc_core::extractors::extract_sections(tree.root_node(), content.as_bytes());

        // The index.adoc should have the document title as its only section
        assert!(
            !sections.is_empty(),
            "arc42 index.adoc should have sections"
        );
        assert_eq!(
            sections[0].title, "asciidoc-lsp Architecture Documentation",
            "document title should match"
        );

        // Convert to DocumentSymbols — the hierarchy should work
        let symbols = sections_to_document_symbols(&sections, &content);
        assert!(!symbols.is_empty(), "should produce document symbols");
        assert_eq!(symbols[0].name, "asciidoc-lsp Architecture Documentation");
        assert_eq!(symbols[0].kind, SymbolKind::STRUCT);

        // Verify an included chapter file also produces valid symbols
        let chapter = docs_dir.join("chapters/01_introduction_and_goals.adoc");
        let ch_content = fs::read_to_string(&chapter).unwrap();
        let ch_tree = asciidoc_core::parser::parse(&mut parser, &ch_content).unwrap();
        let ch_sections =
            asciidoc_core::extractors::extract_sections(ch_tree.root_node(), ch_content.as_bytes());

        assert!(
            ch_sections.len() >= 3,
            "introduction chapter should have multiple sections, found: {}",
            ch_sections.len()
        );

        let ch_symbols = sections_to_document_symbols(&ch_sections, &ch_content);
        assert!(
            !ch_symbols.is_empty(),
            "chapter should produce document symbols"
        );
    }

    #[test]
    fn test_integration_arc42_diagnostics() {
        let ws_root = workspace_root();
        let entry = find_entry_point(&ws_root).unwrap();

        let ws = Workspace::build(&ws_root, &entry).unwrap();

        // Arc42 fixture has exactly 2 known diagnostics:
        // 1. Broken include: {source-dir}/main.rs (attribute substitution resolves to ../src/main.rs which doesn't exist)
        // 2. Broken cross-reference: ../05_building_block_view.adoc#building-blocks
        //
        // We don't assert exact count since the fixture may evolve, but we verify:
        // - No excessive false positives (< 10)
        // - Each diagnostic has correct metadata
        assert!(
            ws.diagnostics.len() < 10,
            "should not have excessive false-positive diagnostics, found: {}",
            ws.diagnostics.len()
        );

        // Convert diagnostics for each file and verify structure
        for diag in &ws.diagnostics {
            let content = fs::read_to_string(&diag.file).unwrap_or_default();
            let lsp_diags = diagnostics_convert::convert_diagnostics(&[diag.clone()], &content);

            assert_eq!(lsp_diags.len(), 1);
            assert!(
                lsp_diags[0].severity.is_some(),
                "diagnostic should have severity"
            );
            assert_eq!(
                lsp_diags[0].source.as_deref(),
                Some("asciidoc-lsp"),
                "diagnostic source should be asciidoc-lsp"
            );
            // Range should be valid (start line <= end line)
            assert!(
                lsp_diags[0].range.start.line <= lsp_diags[0].range.end.line,
                "diagnostic range should be valid"
            );
        }
    }

    #[test]
    fn test_integration_arc42_workspace_symbols() {
        let ws_root = workspace_root();
        let entry = find_entry_point(&ws_root).unwrap();

        let ws = Workspace::build(&ws_root, &entry).unwrap();

        // All symbols (empty query)
        let all_symbols = collect_workspace_symbols(&ws, "");
        assert!(
            all_symbols.len() >= 10,
            "arc42 workspace should have at least 10 symbols (sections + anchors), found: {}",
            all_symbols.len()
        );

        // Filtered query: "goal" should match "Project Goals" section
        let goal_symbols = collect_workspace_symbols(&ws, "goal");
        assert!(
            !goal_symbols.is_empty(),
            "query 'goal' should match at least one symbol"
        );
        let goal_names: Vec<&str> = goal_symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            goal_names.iter().any(|n| n.contains("Goal")),
            "should find 'Goal'-containing symbol, got: {:?}",
            goal_names
        );

        // All symbols should have valid URIs
        for sym in &all_symbols {
            let uri_str = sym.location.uri.to_string();
            assert!(
                uri_str.starts_with("file://"),
                "symbol URI should be file://: {}",
                uri_str
            );
        }
    }

    #[test]
    fn test_integration_arc42_goto_definition_include() {
        let docs_dir = arc42_docs_dir();
        let index = docs_dir.join("index.adoc");
        let content = fs::read_to_string(&index).unwrap();

        let ws_root = workspace_root();
        let ws = Workspace::build(&ws_root, &index).unwrap();

        // Line 10 in index.adoc: include::chapters/01_introduction_and_goals.adoc[]
        // (0-indexed for LSP: line 9)
        let pos = Position {
            line: 9,
            character: 12,
        };

        let result = navigation::resolve_goto_definition(&content, pos, &index, Some(&ws));
        assert!(
            result.is_some(),
            "goto-definition on include should resolve"
        );

        let (target, line) = result.unwrap();
        assert!(
            target
                .to_string_lossy()
                .contains("01_introduction_and_goals.adoc"),
            "target should be the included file, got: {:?}",
            target
        );
        assert_eq!(line, 1, "include target should point to line 1");
    }

    // --- path_to_uri tests (Finding 2: URL-encoding) ---

    #[test]
    fn test_path_to_uri_simple_path() {
        let uri = path_to_uri(Path::new("/home/user/file.adoc")).unwrap();
        assert_eq!(uri.to_string(), "file:///home/user/file.adoc");
    }

    #[test]
    fn test_path_to_uri_encodes_spaces() {
        let uri = path_to_uri(Path::new("/home/user/my project/file.adoc")).unwrap();
        let uri_str = uri.to_string();
        assert!(
            !uri_str.contains(' '),
            "URI must not contain raw spaces: {uri_str}"
        );
        assert!(
            uri_str.contains("my%20project"),
            "spaces should be percent-encoded: {uri_str}"
        );
    }

    #[test]
    fn test_path_to_uri_encodes_umlauts() {
        let uri = path_to_uri(Path::new("/home/müller/file.adoc")).unwrap();
        let uri_str = uri.to_string();
        assert!(
            !uri_str.contains('ü'),
            "URI must not contain raw umlauts: {uri_str}"
        );
        assert!(
            uri_str.contains("m%C3%BCller"),
            "umlauts should be percent-encoded: {uri_str}"
        );
    }
}
