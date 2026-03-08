mod completion;
mod diagnostics_convert;
mod navigation;
mod positions;
mod symbols;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    CompletionOptions, CompletionParams, CompletionResponse, Diagnostic,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverContents, HoverParams, HoverProviderCapability,
    InitializeParams, InitializeResult, InitializedParams, Location, MarkedString, MessageType,
    OneOf, Position, PrepareRenameResponse, Range, ReferenceParams, RenameOptions, RenameParams,
    SaveOptions, ServerCapabilities, TextDocumentPositionParams, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextDocumentSyncOptions, TextDocumentSyncSaveOptions, TextEdit, Uri,
    WorkDoneProgressOptions, WorkspaceEdit, WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use tower_lsp_server::{Client, LanguageServer, LspService, Server};

use asciidoc_core::workspace::Workspace;

// ---------------------------------------------------------------------------
// Document Store (DC-4)
// ---------------------------------------------------------------------------

/// State for an open document. No cached Tree (V-1: `tree_sitter::Tree` is !Send).
#[derive(Debug)]
struct DocumentState {
    content: Arc<String>,
    version: i32,
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

    fn get_document_content(&self, uri: &Uri) -> Option<Arc<String>> {
        let docs = self.documents.lock().expect("documents lock poisoned");
        if let Some(state) = docs.get(uri) {
            Some(Arc::clone(&state.content))
        } else if let Some(path) = uri.to_file_path() {
            std::fs::read_to_string(path.as_ref()).ok().map(Arc::new)
        } else {
            None
        }
    }

    /// Publish diagnostics for all files in the workspace.
    ///
    /// Takes pre-grouped diagnostics, converts them to LSP format,
    /// publishes per-file, and clears stale diagnostics for files
    /// that no longer have any.
    async fn publish_workspace_diagnostics(
        &self,
        diagnostics: &HashMap<PathBuf, Vec<asciidoc_core::types::Diagnostic>>,
    ) {
        // Prepare all conversions without holding locks across await points
        let conversions: Vec<(Uri, Vec<Diagnostic>)> = {
            let docs = self.documents.lock().expect("documents lock poisoned");
            diagnostics
                .iter()
                .filter_map(|(path, file_diags)| {
                    let uri = symbols::path_to_uri(path)?;
                    let content: String = docs.get(&uri).map_or_else(
                        || std::fs::read_to_string(path).unwrap_or_default(),
                        |s| (*s.content).clone(),
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
            *self.root.lock().expect("root lock poisoned") = Some(root.clone());

            // Create workspace and scan for all .adoc files
            let mut ws = Workspace::new(root);
            ws.scan_workspace();
            ws.recalculate_diagnostics();
            *self.workspace.lock().expect("workspace lock poisoned") = Some(ws);
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
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                })),
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

        self.documents.lock().unwrap().insert(
            uri.clone(),
            DocumentState {
                content: Arc::new(content),
                version,
            },
        );

        // Per-file: ensure only this file, then recalculate diagnostics
        if let Some(path) = uri.to_file_path().map(std::borrow::Cow::into_owned) {
            if let Ok(canonical) = path.canonicalize() {
                let overrides = build_content_overrides(&self.documents);
                let diagnostics = {
                    let mut ws = self.workspace.lock().expect("workspace lock poisoned");
                    if let Some(ws) = ws.as_mut() {
                        // Use update_file if already indexed (e.g. from background scan)
                        // to apply editor content over disk content.
                        // Use ensure_file for new files not yet in the workspace.
                        if ws.documents.contains_key(&canonical) {
                            ws.update_file(&canonical, &overrides);
                        } else {
                            ws.ensure_file(&canonical, &overrides);
                        }
                        ws.recalculate_diagnostics();
                        Some(ws.diagnostics.clone())
                    } else {
                        None
                    }
                };
                if let Some(diagnostics) = diagnostics {
                    self.publish_workspace_diagnostics(&diagnostics).await;
                }
            }
        }
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
                    uri.clone(),
                    DocumentState {
                        content: Arc::new(change.text),
                        version: new_version,
                    },
                );
            }
        }

        // Per-file: update only this file, then recalculate diagnostics
        if let Some(path) = uri.to_file_path().map(std::borrow::Cow::into_owned) {
            if let Ok(canonical) = path.canonicalize() {
                let overrides = build_content_overrides(&self.documents);
                let diagnostics = {
                    let mut ws = self.workspace.lock().expect("workspace lock poisoned");
                    if let Some(ws) = ws.as_mut() {
                        ws.update_file(&canonical, &overrides);
                        ws.recalculate_diagnostics();
                        Some(ws.diagnostics.clone())
                    } else {
                        None
                    }
                };
                if let Some(diagnostics) = diagnostics {
                    self.publish_workspace_diagnostics(&diagnostics).await;
                }
            }
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents
            .lock()
            .expect("documents lock poisoned")
            .remove(&uri);

        // Re-index file from disk so gd/gr still work for closed files
        if let Some(path) = uri.to_file_path().map(std::borrow::Cow::into_owned) {
            if let Ok(canonical) = path.canonicalize() {
                let diagnostics = {
                    let mut ws = self.workspace.lock().expect("workspace lock poisoned");
                    if let Some(ws) = ws.as_mut() {
                        ws.update_file(&canonical, &HashMap::new());
                        ws.recalculate_diagnostics();
                        Some(ws.diagnostics.clone())
                    } else {
                        None
                    }
                };
                if let Some(diagnostics) = diagnostics {
                    self.publish_workspace_diagnostics(&diagnostics).await;
                }
            }
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        // Re-parse from disk content (editor may have saved changes not sent via did_change)
        let uri = params.text_document.uri;
        if let Some(path) = uri.to_file_path().map(std::borrow::Cow::into_owned) {
            if let Ok(canonical) = path.canonicalize() {
                let overrides = build_content_overrides(&self.documents);
                let diagnostics = {
                    let mut ws = self.workspace.lock().expect("workspace lock poisoned");
                    if let Some(ws) = ws.as_mut() {
                        ws.update_file(&canonical, &overrides);
                        ws.recalculate_diagnostics();
                        Some(ws.diagnostics.clone())
                    } else {
                        None
                    }
                };
                if let Some(diagnostics) = diagnostics {
                    self.publish_workspace_diagnostics(&diagnostics).await;
                }
            }
        }
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
            tracing::debug!(?uri, "hover: no document content");
            return Ok(None);
        };

        let doc_path = match uri.to_file_path() {
            Some(p) => p.into_owned(),
            None => return Ok(None),
        };

        let ws = self.workspace.lock().expect("workspace lock poisoned");
        tracing::debug!(
            ?doc_path,
            workspace_exists = ws.is_some(),
            "hover: resolving at {}:{}",
            position.line,
            position.character,
        );
        let hover_text = navigation::resolve_hover(&content, position, &doc_path, ws.as_ref());
        tracing::debug!(?hover_text, "hover: result");

        match hover_text {
            Some(text) => Ok(Some(Hover {
                contents: HoverContents::Scalar(MarkedString::String(text)),
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
            .filter_map(|loc| {
                let target_uri = Uri::from_file_path(&loc.path)?;
                let file_content = std::fs::read_to_string(&loc.path).unwrap_or_default();
                let start = positions::to_lsp_position(loc.line, loc.col, &file_content);
                let end = positions::to_lsp_position(loc.line, loc.end_col, &file_content);
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

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = &params.text_document.uri;
        let position = params.position;

        let Some(content) = self.get_document_content(uri) else {
            return Ok(None);
        };

        let doc_path = match uri.to_file_path() {
            Some(p) => p.into_owned(),
            None => return Ok(None),
        };

        let ws = self.workspace.lock().expect("workspace lock poisoned");
        let result = navigation::prepare_rename(&content, position, &doc_path, ws.as_ref());
        drop(ws);

        let Some(r) = result else {
            tracing::debug!("prepare_rename: no result for position {position:?}");
            return Ok(None);
        };

        tracing::debug!(
            "prepare_rename: placeholder={:?} line={} start_col={} end_col={}",
            r.placeholder,
            r.line,
            r.start_col,
            r.end_col
        );

        let start = positions::to_lsp_position(r.line, r.start_col, &content);
        let end = positions::to_lsp_position(r.line, r.end_col, &content);

        tracing::debug!(
            "prepare_rename: LSP range start=({},{}) end=({},{})",
            start.line,
            start.character,
            end.line,
            end.character
        );

        Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
            range: Range { start, end },
            placeholder: r.placeholder,
        }))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let new_name = params.new_name;

        let Some(content) = self.get_document_content(uri) else {
            return Ok(None);
        };

        let doc_path = match uri.to_file_path() {
            Some(p) => p.into_owned(),
            None => return Ok(None),
        };

        let overrides = build_content_overrides(&self.documents);

        let ws = self.workspace.lock().expect("workspace lock poisoned");
        let result =
            navigation::resolve_rename(&content, position, &doc_path, ws.as_ref(), &overrides);
        drop(ws);

        let Some(file_edits) = result else {
            return Ok(None);
        };

        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
        for (path, edits) in file_edits {
            let Some(file_uri) = Uri::from_file_path(&path) else {
                continue;
            };
            let file_content = overrides.get(&path).map_or_else(
                || {
                    std::fs::read_to_string(&path).unwrap_or_else(|e| {
                        tracing::warn!(
                            "failed to read file for rename edit: {}: {e}",
                            path.display()
                        );
                        String::new()
                    })
                },
                |arc| (**arc).clone(),
            );
            let text_edits: Vec<TextEdit> = edits
                .into_iter()
                .map(|(line, start_col, end_col)| {
                    let start = positions::to_lsp_position(line, start_col, &file_content);
                    let end = positions::to_lsp_position(line, end_col, &file_content);
                    TextEdit {
                        range: Range { start, end },
                        new_text: new_name.clone(),
                    }
                })
                .collect();
            changes.insert(file_uri, text_edits);
        }

        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }))
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

/// Build a `HashMap` of file path -> content for all open documents.
/// Used as overrides so the semantic layer reads in-memory content
/// instead of stale disk content.
fn build_content_overrides(
    documents: &Mutex<HashMap<Uri, DocumentState>>,
) -> HashMap<PathBuf, Arc<String>> {
    let docs = documents.lock().expect("documents lock poisoned");
    docs.iter()
        .filter_map(|(uri, state)| {
            uri.to_file_path()
                .map(|path| (path.into_owned(), Arc::clone(&state.content)))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Tracing: write to stderr (LSP uses stdout for protocol).
    // Control via ASCIIDOC_LSP_LOG env var, e.g. ASCIIDOC_LSP_LOG=debug
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing::level_filters::LevelFilter::WARN.into())
                .with_env_var("ASCIIDOC_LSP_LOG")
                .from_env_lossy(),
        )
        .init();

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
    use std::path::Path;
    use std::str::FromStr;

    // --- Document Lifecycle tests ---

    #[test]
    fn test_did_open_stores_document() {
        let docs: Mutex<HashMap<Uri, DocumentState>> = Mutex::new(HashMap::new());
        let uri = Uri::from_str("file:///test.adoc").unwrap();
        let content = "= Title\n\nContent.".to_string();

        docs.lock().unwrap().insert(
            uri.clone(),
            DocumentState {
                content: Arc::new(content.clone()),
                version: 1,
            },
        );

        let stored = docs.lock().unwrap();
        let state = stored.get(&uri).unwrap();
        assert_eq!(*state.content, content);
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
                content: Arc::new("old content".into()),
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
                    content: Arc::new("new content".into()),
                    version: 2,
                },
            );
        }

        let stored = docs.lock().unwrap();
        let state = stored.get(&uri).unwrap();
        assert_eq!(*state.content, "new content");
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
                content: Arc::new("current content".into()),
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
                        content: Arc::new("stale content".into()),
                        version: old_version,
                    },
                );
            }
            // else: discard
        }

        let stored = docs.lock().unwrap();
        let state = stored.get(&uri).unwrap();
        assert_eq!(*state.content, "current content");
        assert_eq!(state.version, 5);
    }

    #[test]
    fn test_did_close_removes_document() {
        let docs: Mutex<HashMap<Uri, DocumentState>> = Mutex::new(HashMap::new());
        let uri = Uri::from_str("file:///test.adoc").unwrap();

        docs.lock().unwrap().insert(
            uri.clone(),
            DocumentState {
                content: Arc::new("content".into()),
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
                content: Arc::new("overridden content".into()),
                version: 1,
            },
        );

        let overrides = build_content_overrides(&docs);
        assert_eq!(overrides.len(), 1);
        assert_eq!(
            overrides.get(Path::new("/tmp/test.adoc")).unwrap().as_str(),
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
            Arc::new("== Modified Chapter B\n\nEdited content.\n".to_string()),
        );

        let ws = Workspace::build_with_overrides(tmp.path(), &a, &overrides);

        // Verify the override content was used
        let b_doc = ws.documents.get(&b_canon).unwrap();
        assert_eq!(b_doc.sections[0].title, "Modified Chapter B");
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
        let entry = arc42_docs_dir().join("index.adoc");

        let ws = Workspace::build(&ws_root, &entry);

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
        let entry = arc42_docs_dir().join("index.adoc");

        let ws = Workspace::build(&ws_root, &entry);

        // Arc42 fixture has exactly 2 known diagnostics:
        // 1. Broken include: {source-dir}/main.rs (attribute substitution resolves to ../src/main.rs which doesn't exist)
        // 2. Broken cross-reference: ../05_building_block_view.adoc#building-blocks
        //
        // We don't assert exact count since the fixture may evolve, but we verify:
        // - No excessive false positives (< 10)
        // - Each diagnostic has correct metadata
        let all_diags = ws.all_diagnostics();
        assert!(
            all_diags.len() < 10,
            "should not have excessive false-positive diagnostics, found: {}",
            all_diags.len()
        );

        // Convert diagnostics for each file and verify structure
        for diag in &all_diags {
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
        let ws = Workspace::build(&ws_root, &index);

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
        let ws = Workspace::build(&ws_root, &index);

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

    // --- Workspace lifecycle tests (adoc-0016: Live Diagnostics) ---

    #[test]
    fn test_new_workspace_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Workspace::new creates an empty workspace — files added via ensure_file
        let ws = Workspace::new(root);
        assert!(ws.documents.is_empty(), "Rebuild returns empty workspace");
    }

    #[test]
    fn test_ensure_file_with_overrides_uses_override_content() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Disk: valid xref
        let index = root.join("index.adoc");
        fs::write(
            &index,
            "= Title\n\n[[valid-anchor]]\n== Section\n\n<<valid-anchor>>\n",
        )
        .unwrap();

        // Override: broken xref (simulates unsaved edit)
        let index_canon = index.canonicalize().unwrap();
        let mut overrides = HashMap::new();
        overrides.insert(
            index_canon.clone(),
            Arc::new("= Title\n\n<<broken-ref>>\n".to_string()),
        );

        let mut ws = Workspace::new(root);
        ws.ensure_file(&index_canon, &overrides);

        // Verify override content was used (doc has broken xref, not valid anchor)
        let doc = ws.documents.get(&index_canon).unwrap();
        assert!(
            doc.xrefs.iter().any(|x| x.target == "broken-ref"),
            "Override content should be used"
        );
    }

    #[test]
    fn test_new_workspace_no_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let ws = Workspace::new(root);
        assert!(ws.documents.is_empty(), "Empty workspace has no documents");
        assert!(
            ws.diagnostics.is_empty(),
            "Empty workspace has no diagnostics"
        );
    }

    #[test]
    fn test_rebuild_preserves_standalone_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let standalone = root.join("standalone.adoc");
        fs::write(&standalone, "= Standalone\n:myattr: val\n\nText.\n").unwrap();

        let overrides = HashMap::new();
        let mut ws = Workspace::new(root);
        assert!(ws.documents.is_empty(), "Rebuild starts empty");

        // Simulate did_open: ensure standalone is added
        let standalone_canon = standalone.canonicalize().unwrap();
        ws.ensure_file(&standalone_canon, &overrides);
        assert_eq!(ws.documents.len(), 1, "Standalone added");

        // Simulate rebuild (e.g. after did_change on another file)
        let mut ws2 = Workspace::new(root);
        assert!(ws2.documents.is_empty(), "Rebuild starts empty again");

        // Re-ensure standalone after workspace recreation
        ws2.ensure_file(&standalone_canon, &overrides);
        assert_eq!(ws2.documents.len(), 1, "Standalone preserved after rebuild");
        assert!(ws2.documents.contains_key(&standalone_canon));
    }

    #[test]
    fn test_did_open_standalone_file() {
        // Simulates what did_open does: rebuild + ensure_file for opened file
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let tests_dir = root.join("tests");
        fs::create_dir_all(&tests_dir).unwrap();
        let standalone = tests_dir.join("standalone.adoc");
        fs::write(
            &standalone,
            "= Standalone\n:myattr: val\n\nText with {myattr}.\n",
        )
        .unwrap();

        let overrides = HashMap::new();
        let mut ws = Workspace::new(root);

        // did_open calls ensure_file for the opened file
        let standalone_canon = standalone.canonicalize().unwrap();
        ws.ensure_file(&standalone_canon, &overrides);

        assert_eq!(ws.documents.len(), 1);
        let doc = ws.documents.get(&standalone_canon).unwrap();
        assert_eq!(doc.attributes.len(), 1);
        assert_eq!(doc.attribute_refs.len(), 1);

        // Config chain should be loaded
        assert_eq!(
            ws.attributes.resolve(&standalone_canon, "myattr"),
            Some("val")
        );
    }

    #[test]
    fn test_prepare_rename_response_json_serialization() {
        // Simulate what the handler produces for [[my-anchor]] on line 3
        let response = PrepareRenameResponse::RangeWithPlaceholder {
            range: Range {
                start: Position {
                    line: 2,
                    character: 2,
                },
                end: Position {
                    line: 2,
                    character: 11,
                },
            },
            placeholder: "my-anchor".to_string(),
        };

        let json = serde_json::to_value(&response).unwrap();
        let obj = json.as_object().unwrap();

        // Verify the JSON has "range" and "placeholder" fields
        assert!(
            obj.contains_key("range"),
            "JSON must have 'range' field: {json}"
        );
        assert!(
            obj.contains_key("placeholder"),
            "JSON must have 'placeholder' field: {json}"
        );

        let range = obj.get("range").unwrap().as_object().unwrap();
        let start = range.get("start").unwrap().as_object().unwrap();
        let end = range.get("end").unwrap().as_object().unwrap();

        assert_eq!(start.get("line").unwrap().as_u64().unwrap(), 2);
        assert_eq!(
            start.get("character").unwrap().as_u64().unwrap(),
            2,
            "start.character must be 2, not 0"
        );
        assert_eq!(end.get("line").unwrap().as_u64().unwrap(), 2);
        assert_eq!(
            end.get("character").unwrap().as_u64().unwrap(),
            11,
            "end.character must be 11"
        );
        assert_eq!(
            obj.get("placeholder").unwrap().as_str().unwrap(),
            "my-anchor"
        );
    }

    // --- adoc-0072 Phase 1: Workspace-Root from LSP root_uri ---

    #[test]
    fn test_workspace_root_from_lsp_root_uri() {
        // Setup: .asciidoctorconfig in parent dir, LSP root_uri points to subdir.
        // The workspace root should be EXACTLY the root_uri, not walked up.
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path();
        fs::write(parent.join(".asciidoctorconfig"), ":imagesdir: images\n").unwrap();

        let sub = parent.join("docs");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("index.adoc"), "= Title\n").unwrap();

        // Workspace::new receives the root_uri from LSP.
        // It should use this path directly as workspace root.
        let ws = Workspace::new(&sub);
        assert_eq!(
            ws.root,
            sub.canonicalize().unwrap(),
            "Workspace root must be the LSP root_uri, not walked up to .asciidoctorconfig"
        );
    }
}
