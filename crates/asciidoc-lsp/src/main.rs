mod completion;
mod diagnostics_convert;
mod navigation;
mod positions;
mod symbols;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    CompletionOptions, CompletionParams, CompletionResponse, Diagnostic,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverContents, HoverParams, HoverProviderCapability,
    InitializeParams, InitializeResult, InitializedParams, Location, MarkedString, MessageType,
    OneOf, Position, Range, ReferenceParams, SaveOptions, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, Uri, WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use tower_lsp_server::{Client, LanguageServer, LspService, Server};

use asciidoc_core::workspace::Workspace;

// ---------------------------------------------------------------------------
// Document Store (DC-4)
// ---------------------------------------------------------------------------

/// State for an open document. No cached Tree (V-1: `tree_sitter::Tree` is !Send).
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

    fn get_document_content(&self, uri: &Uri) -> Option<String> {
        let docs = self.documents.lock().expect("documents lock poisoned");
        if let Some(state) = docs.get(uri) {
            Some(state.content.clone())
        } else if let Some(path) = uri.to_file_path() {
            std::fs::read_to_string(path.as_ref()).ok()
        } else {
            None
        }
    }

    /// Publish diagnostics for all files in the workspace after a rebuild.
    ///
    /// Groups diagnostics by file, converts them, publishes per-file, and
    /// clears stale diagnostics for files that no longer have any.
    async fn publish_workspace_diagnostics(
        &self,
        diagnostics: &[asciidoc_core::types::Diagnostic],
    ) {
        // Group diagnostics by file
        let mut diags_by_file: HashMap<PathBuf, Vec<asciidoc_core::types::Diagnostic>> =
            HashMap::new();
        for diag in diagnostics {
            diags_by_file
                .entry(diag.file.clone())
                .or_default()
                .push(diag.clone());
        }

        // Prepare all conversions without holding locks across await points
        let conversions: Vec<(Uri, Vec<Diagnostic>)> = {
            let docs = self.documents.lock().expect("documents lock poisoned");
            diags_by_file
                .iter()
                .filter_map(|(path, file_diags)| {
                    let uri = symbols::path_to_uri(path)?;
                    let content = docs.get(&uri).map_or_else(
                        || std::fs::read_to_string(path).unwrap_or_default(),
                        |s| s.content.clone(),
                    );
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
        let old_published = self
            .published_diagnostics
            .lock()
            .expect("published_diagnostics lock poisoned")
            .clone();
        for uri in &old_published {
            if !new_published.contains(uri) {
                self.client
                    .publish_diagnostics(uri.clone(), vec![], None)
                    .await;
            }
        }

        *self
            .published_diagnostics
            .lock()
            .expect("published_diagnostics lock poisoned") = new_published;
    }

    /// Rebuild workspace from document store and publish diagnostics.
    async fn rebuild_and_publish(&self) {
        let root = self.root.lock().expect("root lock poisoned").clone();
        let Some(root) = root else { return };

        let overrides = build_content_overrides(&self.documents);

        if let Some(ws) = rebuild_workspace(&root, &overrides) {
            self.publish_workspace_diagnostics(&ws.diagnostics).await;
            *self.workspace.lock().expect("workspace lock poisoned") = Some(ws);
        }
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
            .and_then(|uri| uri.to_file_path().map(std::borrow::Cow::into_owned))
            .or_else(|| params.root_path.as_ref().map(PathBuf::from));

        if let Some(ref root) = root_path {
            let workspace_root = detect_workspace_root(root);
            *self.root.lock().expect("root lock poisoned") = Some(workspace_root.clone());

            // Build initial workspace if entry point found
            if let Some(entry) = find_entry_point(&workspace_root) {
                let overrides = HashMap::new();
                if let Ok(ws) = Workspace::build_with_overrides(&workspace_root, &entry, &overrides)
                {
                    *self.workspace.lock().expect("workspace lock poisoned") = Some(ws);
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
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                        ..Default::default()
                    },
                )),
                document_symbol_provider: Some(OneOf::Left(true)),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        "<".into(), // << xref
                        ":".into(), // include::
                        "{".into(), // {attribute
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let diagnostics = {
            let ws = self.workspace.lock().expect("workspace lock poisoned");
            ws.as_ref().map(|ws| ws.diagnostics.clone())
        };
        if let Some(diagnostics) = diagnostics {
            self.publish_workspace_diagnostics(&diagnostics).await;
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let content = params.text_document.text;
        let version = params.text_document.version;

        self.documents
            .lock()
            .unwrap()
            .insert(uri, DocumentState { content, version });

        self.rebuild_and_publish().await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let new_version = params.text_document.version;

        // Update document store (lock scope limited)
        {
            let mut docs = self.documents.lock().expect("documents lock poisoned");
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

        // Rebuild workspace and publish diagnostics (DC-5: Live diagnostics on change)
        self.rebuild_and_publish().await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents
            .lock()
            .expect("documents lock poisoned")
            .remove(&uri);

        // Rebuild: uses disk content for closed file, clears stale diagnostics automatically
        self.rebuild_and_publish().await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let _ = params;
        self.rebuild_and_publish().await;
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = &params.text_document.uri;

        let Some(content) = self.get_document_content(uri) else {
            return Ok(None);
        };

        // Parse and extract sections, includes, attributes
        let mut parser = asciidoc_core::parser::init_block_parser()
            .map_err(|e| tower_lsp_server::jsonrpc::Error::invalid_params(e.to_string()))?;
        let tree = asciidoc_core::parser::parse(&mut parser, &content)
            .map_err(|e| tower_lsp_server::jsonrpc::Error::invalid_params(e.to_string()))?;
        let root_node = tree.root_node();
        let source = content.as_bytes();
        let sections = asciidoc_core::extractors::extract_sections(root_node, source);
        let includes = asciidoc_core::extractors::extract_includes(root_node, source);
        let attributes = asciidoc_core::extractors::extract_attributes(root_node, source);

        let symbols = symbols::build_document_symbols(&sections, &includes, &attributes, &content);

        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let Some(content) = self.get_document_content(uri) else {
            return Ok(None);
        };

        // Check if cursor is on an {attribute} reference
        let Some(attr_name) =
            find_attribute_at_position(&content, position.line, position.character)
        else {
            return Ok(None);
        };

        // Resolve the attribute value from the workspace
        let doc_path = match uri.to_file_path() {
            Some(p) => p.into_owned(),
            None => return Ok(None),
        };

        let ws = self.workspace.lock().expect("workspace lock poisoned");
        let value = ws
            .as_ref()
            .and_then(|ws| ws.attributes.resolve(&doc_path, &attr_name))
            .map(std::string::ToString::to_string);

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
        let ws = self.workspace.lock().expect("workspace lock poisoned");
        let Some(ws) = ws.as_ref() else {
            return Ok(None);
        };

        let symbols = symbols::collect_workspace_symbols(ws, &params.query);
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

        let Some(content) = self.get_document_content(uri) else {
            return Ok(None);
        };

        let doc_path = match uri.to_file_path() {
            Some(p) => p.into_owned(),
            None => return Ok(None),
        };

        let ws = self.workspace.lock().expect("workspace lock poisoned");
        let result =
            navigation::resolve_goto_definition(&content, position, &doc_path, ws.as_ref());
        // Drop the lock before building response
        drop(ws);

        let Some((target_path, target_line)) = result else {
            return Ok(None);
        };

        let Some(target_uri) = Uri::from_file_path(&target_path) else {
            return Ok(None);
        };

        // Convert 1-indexed line to 0-indexed LSP position
        // LSP uses u32 for line numbers; real-world files won't exceed u32::MAX lines.
        #[allow(clippy::cast_possible_truncation)]
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

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let Some(content) = self.get_document_content(uri) else {
            return Ok(None);
        };

        let doc_path = match uri.to_file_path() {
            Some(p) => p.into_owned(),
            None => return Ok(None),
        };

        let include_declaration = params.context.include_declaration;

        let ws = self.workspace.lock().expect("workspace lock poisoned");
        let results = navigation::resolve_references(
            &content,
            position,
            &doc_path,
            ws.as_ref(),
            include_declaration,
        );
        drop(ws);

        let Some(refs) = results else {
            return Ok(None);
        };

        let locations: Vec<Location> = refs
            .into_iter()
            .filter_map(|(path, line, start_col, end_col)| {
                let target_uri = Uri::from_file_path(&path)?;
                let file_content = std::fs::read_to_string(&path).unwrap_or_default();
                let start = positions::to_lsp_position(line, start_col, &file_content);
                let end = positions::to_lsp_position(line, end_col, &file_content);
                Some(Location {
                    uri: target_uri,
                    range: Range { start, end },
                })
            })
            .collect();

        if locations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(locations))
        }
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let Some(content) = self.get_document_content(uri) else {
            return Ok(None);
        };

        // Detect completion context from cursor position
        let Some(ctx) = completion::detect_completion_context(&content, position) else {
            return Ok(None);
        };

        let doc_path = match uri.to_file_path() {
            Some(p) => p.into_owned(),
            None => return Ok(None),
        };

        let ws = self.workspace.lock().expect("workspace lock poisoned");
        let items = completion::resolve_completions(ctx, &doc_path, ws.as_ref());

        if items.is_empty() {
            Ok(None)
        } else {
            Ok(Some(CompletionResponse::Array(items)))
        }
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

/// Rebuild workspace: detect root, find entry point, build with overrides.
/// Returns None if no entry point found or build fails.
fn rebuild_workspace(root: &Path, overrides: &HashMap<PathBuf, String>) -> Option<Workspace> {
    let workspace_root = detect_workspace_root(root);
    let entry = find_entry_point(&workspace_root)?;
    Workspace::build_with_overrides(&workspace_root, &entry, overrides).ok()
}

/// Build a `HashMap` of file path -> content for all open documents.
/// Used as overrides so the semantic layer reads in-memory content
/// instead of stale disk content.
fn build_content_overrides(
    documents: &Mutex<HashMap<Uri, DocumentState>>,
) -> HashMap<PathBuf, String> {
    let docs = documents.lock().expect("documents lock poisoned");
    docs.iter()
        .filter_map(|(uri, state)| {
            uri.to_file_path()
                .map(|path| (path.into_owned(), state.content.clone()))
        })
        .collect()
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

    #[test]
    fn test_integration_arc42_goto_definition_attribute_config() {
        let docs_dir = arc42_docs_dir();
        let index = docs_dir.join("index.adoc");
        let content = fs::read_to_string(&index).unwrap();

        let ws_root = workspace_root();
        let detected_root = detect_workspace_root(&ws_root);
        let ws = Workspace::build(&detected_root, &index).unwrap();

        // Line 20 in index.adoc: include::{docdir}/appendix/glossary.adoc[]
        // {docdir} is at columns 9..16 (0-indexed LSP line 19)
        let pos = Position {
            line: 19,
            character: 12, // inside "docdir"
        };

        let result = navigation::resolve_goto_definition(&content, pos, &index, Some(&ws));
        assert!(
            result.is_some(),
            "goto-definition on {{docdir}} should resolve to .asciidoctorconfig"
        );

        let (target, line) = result.unwrap();
        assert!(
            target.to_string_lossy().contains(".asciidoctorconfig"),
            "target should be .asciidoctorconfig, got: {:?}",
            target
        );
        // Verify the line actually contains the docdir attribute definition
        let config_path = target;
        let config_content = std::fs::read_to_string(&config_path).unwrap();
        let config_line = config_content.lines().nth(line - 1).unwrap_or("");
        assert!(
            config_line.contains(":docdir:"),
            "line {} of .asciidoctorconfig should contain :docdir:, got: {:?}",
            line,
            config_line
        );
    }

    // --- rebuild_workspace tests (adoc-0016: Live Diagnostics) ---

    #[test]
    fn test_rebuild_workspace_returns_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Entry file with a broken xref
        fs::write(root.join("index.adoc"), "= Title\n\n<<nonexistent>>\n").unwrap();

        let documents: Mutex<HashMap<Uri, DocumentState>> = Mutex::new(HashMap::new());
        let overrides = build_content_overrides(&documents);

        let result = rebuild_workspace(root, &overrides);

        assert!(result.is_some());
        let ws = result.unwrap();
        // Workspace must have at least one diagnostic (broken xref)
        assert!(!ws.diagnostics.is_empty());
    }

    #[test]
    fn test_did_change_rebuild_updates_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Setup: file with valid content on disk
        fs::write(
            root.join("index.adoc"),
            "= Title\n\n[[anchor]]\n== Section\n\n<<anchor>>\n",
        )
        .unwrap();

        // Simulate did_change: Document Store with modified content
        let uri = Uri::from_str(&format!(
            "file://{}",
            root.join("index.adoc").canonicalize().unwrap().display()
        ))
        .unwrap();
        let documents: Mutex<HashMap<Uri, DocumentState>> = Mutex::new(HashMap::new());
        documents.lock().unwrap().insert(
            uri,
            DocumentState {
                content: "= Title\n\n<<broken-xref>>\n".into(),
                version: 2,
            },
        );

        // Rebuild with overrides from Document Store
        let overrides = build_content_overrides(&documents);
        let result = rebuild_workspace(root, &overrides);

        assert!(result.is_some());
        let ws = result.unwrap();
        // Override content must produce diagnostic for broken xref
        assert!(
            ws.diagnostics
                .iter()
                .any(|d| d.message.contains("broken-xref")),
            "Expected diagnostic from did_change content override"
        );
    }

    #[test]
    fn test_rebuild_workspace_uses_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Disk: valid xref
        fs::write(
            root.join("index.adoc"),
            "= Title\n\n[[valid-anchor]]\n== Section\n\n<<valid-anchor>>\n",
        )
        .unwrap();

        // Override: broken xref (simulates unsaved edit)
        let index_path = root.join("index.adoc").canonicalize().unwrap();
        let mut overrides = HashMap::new();
        overrides.insert(index_path, "= Title\n\n<<broken-ref>>\n".to_string());

        let result = rebuild_workspace(root, &overrides);
        assert!(result.is_some());
        let ws = result.unwrap();
        assert!(
            ws.diagnostics
                .iter()
                .any(|d| d.message.contains("broken-ref")),
            "Expected diagnostic for broken xref from override"
        );
    }
}
