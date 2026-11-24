#[cfg(test)]
mod agreement;
mod completion;
mod diagnostics_convert;
mod navigation;
mod positions;
mod present;
mod preview;
mod preview_converter;
mod preview_target;
mod semantic_tokens;
mod symbols;
#[cfg(test)]
mod test_support;
mod version;
mod watchdog;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use tower_lsp_server::jsonrpc::{self, Result};
use tower_lsp_server::ls_types::{
    CodeAction, CodeActionKind, CodeActionOptions, CodeActionOrCommand, CodeActionParams,
    CodeActionProviderCapability, CodeActionResponse, CompletionOptions, CompletionParams,
    CompletionResponse, Diagnostic, DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidChangeWatchedFilesRegistrationOptions, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentSymbolParams,
    DocumentSymbolResponse, ExecuteCommandOptions, ExecuteCommandParams, FileChangeType, FileEvent,
    FileSystemWatcher, GlobPattern, GotoDefinitionParams, GotoDefinitionResponse, Hover,
    HoverContents, HoverParams, HoverProviderCapability, InitializeParams, InitializeResult,
    InitializedParams, Location, MarkedString, MessageType, OneOf, Position, PrepareRenameResponse,
    Range, ReferenceParams, Registration, RenameOptions, RenameParams, SaveOptions, SemanticTokens,
    SemanticTokensFullOptions, SemanticTokensOptions, SemanticTokensParams, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, ServerInfo, ShowDocumentParams,
    TextDocumentPositionParams, TextDocumentSyncCapability, TextDocumentSyncKind,
    TextDocumentSyncOptions, TextDocumentSyncSaveOptions, TextEdit, Uri, WorkDoneProgressOptions,
    WorkspaceEdit, WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use tower_lsp_server::{Client, LanguageServer, LspService, Server};

use asciidoc_core::diagnostics::collect_diagnostics;
use asciidoc_core::workspace::Workspace;

/// How long the diagnostics wait after an edit: a newer edit within this
/// time starts the wait anew, so a burst of keystrokes is computed once.
const DIAGNOSTICS_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);

// ---------------------------------------------------------------------------
// Document Store (DC-4)
// ---------------------------------------------------------------------------

/// State for an open document. No cached Tree (V-1: `tree_sitter::Tree` is !Send).
#[derive(Debug)]
struct DocumentState {
    content: Arc<String>,
    version: i32,
}

/// Core diagnostics by file, as `collect_diagnostics` groups them.
type DiagnosticsByFile = HashMap<PathBuf, Vec<asciidoc_core::types::Diagnostic>>;

/// The diagnostics of one recomputation and the files whose text changed
/// before it, which are published whether or not their diagnostics did.
#[derive(Debug)]
struct Recomputed {
    /// The diagnostics generation this recomputation advanced to: a later
    /// recomputation has a higher one.
    generation: u64,
    diagnostics: DiagnosticsByFile,
    content_changed: HashSet<PathBuf>,
}

/// What the client was last sent.
#[derive(Debug, Default)]
struct Published {
    /// The generation of the newest recomputation published.
    generation: u64,
    /// Its diagnostics, by file; a file without any is absent.
    diagnostics: DiagnosticsByFile,
}

// ---------------------------------------------------------------------------
// AsciiDocLsp Server
// ---------------------------------------------------------------------------

/// The server. Cheap to clone: the background workspace scan keeps a clone
/// to install its result.
#[derive(Debug, Clone)]
struct AsciiDocLsp(Arc<ServerState>);

impl std::ops::Deref for AsciiDocLsp {
    type Target = ServerState;

    fn deref(&self) -> &ServerState {
        &self.0
    }
}

#[derive(Debug)]
struct ServerState {
    client: Client,
    documents: Mutex<HashMap<Uri, DocumentState>>,
    workspace: Mutex<Option<Workspace>>,
    root: Mutex<Option<PathBuf>>,
    /// Diagnostics of the workspace as of the last recomputation, by file.
    diagnostics: Mutex<DiagnosticsByFile>,
    /// What the client was last sent. A file is published only when its
    /// diagnostics differ from these, or its text changed. Held from the
    /// comparison through the last send, so publications do not interleave.
    published: tokio::sync::Mutex<Published>,
    /// Files whose text changed since the last recomputation: their
    /// positions are converted against the text, so they are published
    /// even with the same diagnostics. Written and taken under the
    /// `workspace` lock.
    content_changed: Mutex<HashSet<PathBuf>>,
    /// Whether the client supports dynamic registration of
    /// `workspace/didChangeWatchedFiles`. Set in `initialize()`,
    /// consumed in `initialized()`.
    can_request_file_watcher: Mutex<bool>,
    /// Whether the client handles `window/showDocument`, which a click in
    /// the preview turns into. Set in `initialize()`.
    can_show_document: Mutex<bool>,
    /// File changes reported while the workspace scan runs, replayed when
    /// the workspace is installed. `None` while no scan runs: before it, without
    /// a workspace root, and once the workspace is installed. Lock order:
    /// `workspace`, then `pending_changes`; never held with `documents`.
    pending_changes: Mutex<Option<Vec<ClassifiedEvents>>>,
    /// `false` while a workspace scan runs. Requests that read the workspace
    /// wait for `true`, so they do not answer from a workspace that is not
    /// there yet.
    workspace_ready: tokio::sync::watch::Sender<bool>,
    /// Counts the edits and recomputations of the diagnostics. A debounced
    /// recomputation runs only while it still holds the latest value: a
    /// newer edit, or a recomputation in between, makes it redundant. Read
    /// and written under the `workspace` lock.
    diagnostics_generation: AtomicU64,
    preview: PreviewState,
    /// The task that installs the scanned workspace, for tests to await.
    #[cfg(test)]
    scan: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The scan thread, for tests to wait until the scan has read the disk.
    #[cfg(test)]
    scan_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// How often the diagnostics were recomputed, for tests to count.
    #[cfg(test)]
    recalculations: std::sync::atomic::AtomicUsize,
    /// Every diagnostics publication in order, for tests to read.
    #[cfg(test)]
    publications: Mutex<Vec<(Uri, Vec<Diagnostic>)>>,
}

/// What the live preview keeps between requests. Each field has its own
/// lock, as they were separate fields of the server before.
#[derive(Debug, Default)]
struct PreviewState {
    /// The preview server; `None` until the first
    /// [`PREVIEW_OPEN_COMMAND`].
    server: Mutex<Option<Arc<preview::Preview>>>,
    /// Serialises the start of the preview server, which awaits.
    start: tokio::sync::Mutex<()>,
    /// The document the preview follows: the last one named by the open
    /// command or by [`CURSOR_MOVED_METHOD`].
    active: Mutex<Option<Uri>>,
    /// What the preview converts for the active document, as the last open
    /// command set it.
    mode: Mutex<preview_target::PreviewMode>,
    /// The document of the last conversion request: the top-level document
    /// in whole-document mode, else the active one.
    shown: Mutex<Option<PathBuf>>,
    /// The preview's TOC choice, for every conversion; set by
    /// [`PREVIEW_TOC_COMMAND`] and by the page.
    toc: Mutex<preview_converter::TocMode>,
}

impl PreviewState {
    /// The running preview server, if one was started.
    fn running(&self) -> Option<Arc<preview::Preview>> {
        self.server.lock().clone()
    }

    /// The running preview server and the document it follows; `None` while
    /// either is missing.
    fn following(&self) -> Option<(Arc<preview::Preview>, Uri)> {
        let preview = self.running()?;
        let active = self.active.lock().clone()?;
        Some((preview, active))
    }
}

impl AsciiDocLsp {
    fn new(client: Client) -> Self {
        Self(Arc::new(ServerState {
            client,
            documents: Mutex::new(HashMap::new()),
            workspace: Mutex::new(None),
            root: Mutex::new(None),
            diagnostics: Mutex::new(HashMap::new()),
            published: tokio::sync::Mutex::new(Published::default()),
            content_changed: Mutex::new(HashSet::new()),
            can_request_file_watcher: Mutex::new(false),
            can_show_document: Mutex::new(false),
            pending_changes: Mutex::new(None),
            workspace_ready: tokio::sync::watch::Sender::new(true),
            diagnostics_generation: AtomicU64::new(0),
            preview: PreviewState::default(),
            #[cfg(test)]
            scan: Mutex::new(None),
            #[cfg(test)]
            scan_thread: Mutex::new(None),
            #[cfg(test)]
            recalculations: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            publications: Mutex::new(Vec::new()),
        }))
    }

    /// The running preview server, started on first use. Fails with a hint
    /// if Asciidoctor is not installed.
    async fn ensure_preview(&self) -> Result<Arc<preview::Preview>> {
        if let Some(preview) = self.preview.running() {
            return Ok(preview);
        }
        let _starting = self.preview.start.lock().await;
        if let Some(preview) = self.preview.running() {
            return Ok(preview);
        }
        let root = self.root.lock().clone();
        let mut converter =
            preview_converter::AsciidoctorConverter::new().map_err(preview_error)?;
        if let Some(root) = &root {
            converter = converter.with_root(root.clone());
        }
        let stylesheet = converter
            .default_stylesheet()
            .await
            .map_err(preview_error)?;
        let preview =
            preview::Preview::start(converter, preview_converter::DEBOUNCE, stylesheet, root)
                .await
                .map_err(preview_error)?;
        let preview = Arc::new(preview);
        if let Some(actions) = preview.take_actions() {
            tokio::spawn(self.clone().forward_page_actions(actions));
        }
        *self.preview.server.lock() = Some(Arc::clone(&preview));
        Ok(preview)
    }

    /// Hand the page's actions to the editor, one at a time, until the
    /// preview server goes away.
    async fn forward_page_actions(
        self,
        mut actions: tokio::sync::mpsc::Receiver<preview::PageAction>,
    ) {
        while let Some(action) = actions.recv().await {
            if let preview::PageAction::Toc(mode) = action {
                self.set_preview_toc(mode);
                continue;
            }
            let active = self.preview.active.lock().clone();
            let client_uris = active
                .as_ref()
                .map_or_else(HashMap::new, |active| self.client_uris(active));
            let can_show_document = *self.can_show_document.lock();
            match editor_request(action, active.as_ref(), &client_uris, can_show_document) {
                Some(EditorRequest::ShowDocument(params)) => {
                    if let Err(error) = self.client.show_document(params).await {
                        tracing::warn!(?error, "window/showDocument for a preview click failed");
                    }
                }
                Some(EditorRequest::RevealLine(params)) => {
                    self.client.send_notification::<RevealLine>(params).await;
                }
                None => {}
            }
        }
    }

    /// The texts of the open buffers by canonical path.
    fn open_buffers(&self) -> HashMap<PathBuf, Arc<String>> {
        self.documents
            .lock()
            .iter()
            .filter_map(|(uri, state)| Some((document_path(uri)?, Arc::clone(&state.content))))
            .collect()
    }

    /// The conversion that shows `uri` in the current mode.
    ///
    /// Whole-document mode converts the top-level document that includes the
    /// file (the one on show if several do, else the first in path order; a
    /// file that nothing includes is its own). Whichever document it is, its
    /// text and the texts of the open buffers it includes come from the
    /// editor, the rest from disk, and it gets the attributes of its own
    /// config chain.
    fn preview_request(&self, uri: &Uri) -> Option<preview_converter::ConvertRequest> {
        let active = document_path(uri)?;
        let buffers = self.open_buffers();
        let shown = self.preview.shown.lock().clone();
        let whole = *self.preview.mode.lock() == preview_target::PreviewMode::Document;
        let guard = self.workspace.lock();
        let workspace = guard.as_ref();
        let file = if whole {
            let tops = workspace.map_or_default(|ws| ws.including_top_level_documents(&active));
            preview_target::choose_top_level(&tops, shown.as_deref()).unwrap_or(active)
        } else {
            active
        };
        let text = match buffers.get(&file) {
            Some(text) => text.as_str().to_owned(),
            None => asciidoc_core::source_text::read_adoc_file(&file).ok()?,
        };
        let read = workspace.map_or_default(|ws| ws.documents_read_by(&file));
        let overlay = read
            .into_iter()
            .filter(|path| *path != file)
            .filter_map(|path| {
                let text = buffers.get(&path)?.as_str().to_owned();
                Some((path, text))
            })
            .collect();
        let attributes = workspace.map_or_default(|ws| ws.attributes().config_attributes(&file));
        Some(preview_converter::ConvertRequest {
            file,
            text,
            attributes,
            overlay,
            toc: Some(preview_converter::TocInstruction {
                mode: *self.preview.toc.lock(),
                previous: None,
            }),
        })
    }

    /// Queue `request` and remember its document as the one on show.
    fn send_preview_request(
        &self,
        preview: &preview::Preview,
        request: preview_converter::ConvertRequest,
    ) {
        *self.preview.shown.lock() = Some(request.file.clone());
        preview.request_convert(request);
    }

    /// A buffer's text changed: convert again if it is the active one or part
    /// of the document on show.
    fn preview_buffer_changed(&self, uri: &Uri) {
        let Some((preview, active)) = self.preview.following() else {
            return;
        };
        let affects = active == *uri || {
            let (Some(changed), Some(shown)) =
                (document_path(uri), self.preview.shown.lock().clone())
            else {
                return;
            };
            self.workspace
                .lock()
                .as_ref()
                .is_some_and(|ws| ws.documents_read_by(&shown).contains(&changed))
        };
        if affects {
            if let Some(request) = self.preview_request(&active) {
                self.send_preview_request(&preview, request);
            }
        }
    }

    /// Whether the document on show reads one of the `changed` files. Callers
    /// ask before and after the watcher events reach the workspace: a deleted
    /// file leaves the read set, a created one only enters it.
    fn preview_reads_any(&self, changed: &[PathBuf]) -> bool {
        if self.preview.server.lock().is_none() || self.preview.active.lock().is_none() {
            return false;
        }
        let Some(shown) = self.preview.shown.lock().clone() else {
            return false;
        };
        self.workspace.lock().as_ref().is_some_and(|ws| {
            let read = ws.documents_read_by(&shown);
            changed.iter().any(|path| read.contains(path))
        })
    }

    /// Files changed on disk and the document on show reads one of them
    /// ([`Self::preview_reads_any`]): convert again.
    fn preview_files_changed(&self) {
        let Some((preview, active)) = self.preview.following() else {
            return;
        };
        if let Some(request) = self.preview_request(&active) {
            self.send_preview_request(&preview, request);
        }
    }

    /// `workspace/executeCommand` [`PREVIEW_OPEN_COMMAND`]: start the preview
    /// server if needed, make the document the active one and answer with the
    /// page URL as `{"url": ...}`.
    async fn preview_open(
        &self,
        arguments: Vec<serde_json::Value>,
    ) -> Result<Option<serde_json::Value>> {
        let (uri, mode) = preview_target::parse_open_arguments(arguments, PREVIEW_OPEN_COMMAND)
            .map_err(jsonrpc::Error::invalid_params)?;
        let preview = self.ensure_preview().await?;
        *self.preview.mode.lock() = mode;
        *self.preview.active.lock() = Some(uri.clone());
        self.workspace_ready().await;
        if let Some(request) = self.preview_request(&uri) {
            self.send_preview_request(&preview, request);
        }
        Ok(Some(serde_json::json!({ "url": preview.url() })))
    }

    /// `workspace/executeCommand` [`PREVIEW_DEBUG_COMMAND`]: switch the
    /// scroll-sync debug log on or off (answers `{"enabled": bool}`) or show
    /// it (answers [`preview::Preview::debug_log`]).
    fn preview_debug(&self, arguments: &[serde_json::Value]) -> Result<Option<serde_json::Value>> {
        let action = arguments.first().and_then(serde_json::Value::as_str);
        let Some(action @ ("on" | "off" | "show")) = action else {
            return Err(jsonrpc::Error::invalid_params(format!(
                "{PREVIEW_DEBUG_COMMAND}: expected \"on\", \"off\" or \"show\""
            )));
        };
        let Some(preview) = self.preview.running() else {
            return Err(jsonrpc::Error::invalid_params(format!(
                "{PREVIEW_DEBUG_COMMAND}: no preview is open"
            )));
        };
        if action == "show" {
            return Ok(Some(preview.debug_log()));
        }
        let enabled = action == "on";
        preview.set_debug(enabled);
        Ok(Some(serde_json::json!({ "enabled": enabled })))
    }

    /// `workspace/executeCommand` [`PREVIEW_TOC_COMMAND`]: set the preview's
    /// TOC mode. Fails while no preview is open.
    fn preview_toc(&self, arguments: &[serde_json::Value]) -> Result<Option<serde_json::Value>> {
        let mode = match arguments {
            [mode] => serde_json::from_value(mode.clone()).ok(),
            _ => None,
        };
        let Some(mode) = mode else {
            return Err(jsonrpc::Error::invalid_params(format!(
                "{PREVIEW_TOC_COMMAND}: expected \"auto\", \"off\", \"left\" or \"right\""
            )));
        };
        if self.preview.running().is_none() {
            return Err(jsonrpc::Error::invalid_params(format!(
                "{PREVIEW_TOC_COMMAND}: no preview is open"
            )));
        }
        self.set_preview_toc(mode);
        Ok(None)
    }

    /// The one place the TOC mode changes, from the editor or the page:
    /// keep it, tell every page, and convert the document on show again.
    fn set_preview_toc(&self, mode: preview_converter::TocMode) {
        *self.preview.toc.lock() = mode;
        let Some(preview) = self.preview.running() else {
            return;
        };
        preview.set_toc(mode);
        let active = self.preview.active.lock().clone();
        if let Some(request) = active.and_then(|active| self.preview_request(&active)) {
            self.send_preview_request(&preview, request);
        }
    }

    /// Notification [`CURSOR_MOVED_METHOD`]: the editor's cursor is on
    /// `params.line` (0-based) of `params.uri`. Ignored while no preview is
    /// open. A different document becomes the shown one.
    #[allow(
        clippy::unused_async,
        reason = "`LspServiceBuilder::custom_method` takes an async handler"
    )]
    async fn cursor_moved(&self, params: CursorMovedParams) {
        let Some(preview) = self.preview.running() else {
            return;
        };
        let Some(file) = document_path(&params.uri) else {
            return;
        };
        let switched = {
            let mut active = self.preview.active.lock();
            let switched = active.as_ref() != Some(&params.uri);
            if switched {
                *active = Some(params.uri.clone());
            }
            switched
        };
        if switched {
            // The page already shows the document if the new active one is
            // part of it.
            if let Some(request) = self.preview_request(&params.uri) {
                if self.preview.shown.lock().as_ref() != Some(&request.file) {
                    self.send_preview_request(&preview, request);
                }
            }
        }
        preview.set_cursor(&file, params.line);
    }

    /// Quick-fixes for the diagnostics of `uri` that touch `range`, taken from
    /// the diagnostics the server last computed rather than the client's copy.
    fn quick_fix_actions(&self, uri: &Uri, range: Range, content: &str) -> CodeActionResponse {
        let Some(doc_path) = document_path(uri) else {
            return Vec::new();
        };
        let guard = self.workspace.lock();
        let Some(ws) = guard.as_ref() else {
            return Vec::new();
        };
        let diagnostics = self.diagnostics.lock();
        let mut actions = Vec::new();
        for core_diagnostic in diagnostics.get(&doc_path).map_or(&[][..], Vec::as_slice) {
            let lsp_diagnostic = diagnostics_convert::convert_one(core_diagnostic, content);
            let touches =
                lsp_diagnostic.range.start <= range.end && range.start <= lsp_diagnostic.range.end;
            if !touches {
                continue;
            }
            for fix in asciidoc_core::quick_fix::quick_fixes(ws, &doc_path, core_diagnostic) {
                let edit_range = Range {
                    start: positions::to_lsp_position(fix.line, fix.start_col, content),
                    end: positions::to_lsp_position(fix.line, fix.end_col, content),
                };
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Change spelling to '{}'", fix.new_text),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![lsp_diagnostic.clone()]),
                    is_preferred: fix.preferred.then_some(true),
                    edit: Some(WorkspaceEdit {
                        changes: Some(HashMap::from([(
                            uri.clone(),
                            vec![TextEdit {
                                range: edit_range,
                                new_text: fix.new_text,
                            }],
                        )])),
                        ..Default::default()
                    }),
                    ..Default::default()
                }));
            }
        }
        actions
    }

    /// Compute the diagnostics map after applying a batch of watcher events.
    /// Returns `None` when nothing relevant happened (no events, all events
    /// belonged to open buffers, or the workspace has not been initialized);
    /// the caller skips publishing in that case.
    fn compute_watched_files_update(
        &self,
        params: DidChangeWatchedFilesParams,
    ) -> Option<Recomputed> {
        let classified = classify_events(params.changes, &self.open_buffer_paths())?;
        self.apply_watcher_events(classified)
    }

    /// Apply a classified batch of watcher events to the workspace and return
    /// fresh diagnostics. Returns `None` if the workspace is not initialized;
    /// during the scan the batch is kept for the install.
    fn apply_watcher_events(&self, classified: ClassifiedEvents) -> Option<Recomputed> {
        // Open buffers are read before the workspace lock is taken, as in
        // the text-sync handlers.
        let open = classified
            .config_changed
            .then(|| open_contents(&self.documents.lock()));
        let mut guard = self.workspace.lock();
        let Some(ws) = guard.as_mut() else {
            self.defer_until_installed(classified);
            return None;
        };
        let reparsed = apply_classified_events(ws, classified, open.as_ref());
        self.content_changed.lock().extend(reparsed);
        Some(self.recompute_diagnostics(ws))
    }

    /// Keep `changes` for the install if the workspace scan runs. The caller
    /// holds the workspace lock and found no workspace: the install, which
    /// takes the queue under that lock, cannot have run in between.
    fn defer_until_installed(&self, changes: ClassifiedEvents) {
        if let Some(pending) = self.pending_changes.lock().as_mut() {
            pending.push(changes);
        }
    }

    /// Snapshot of all paths that correspond to an open editor buffer,
    /// including canonicalized resolutions. Watcher events for these paths
    /// are ignored — LSP text-sync is the authoritative source.
    fn open_buffer_paths(&self) -> HashSet<PathBuf> {
        let docs = self.documents.lock();
        let mut set = HashSet::new();
        for uri in docs.keys() {
            let Some(path) = uri.to_file_path() else {
                continue;
            };
            let path: PathBuf = path.into_owned();
            if let Ok(canonical) = path.canonicalize() {
                set.insert(canonical);
            }
            set.insert(path);
        }
        set
    }

    /// URIs the client addresses its open documents by, keyed by
    /// [`document_path`]. `request` wins over another buffer of the same file.
    fn client_uris(&self, request: &Uri) -> HashMap<PathBuf, Uri> {
        let docs = self.documents.lock();
        docs.keys()
            .chain(std::iter::once(request))
            .filter_map(|uri| Some((document_path(uri)?, uri.clone())))
            .collect()
    }

    fn get_document_content(&self, uri: &Uri) -> Option<Arc<String>> {
        let docs = self.documents.lock();
        if let Some(state) = docs.get(uri) {
            Some(Arc::clone(&state.content))
        } else if let Some(path) = uri.to_file_path() {
            asciidoc_core::source_text::read_adoc_file(path.as_ref())
                .ok()
                .map(Arc::new)
        } else {
            None
        }
    }

    /// Bring the workspace model up to date, then recompute its diagnostics,
    /// keep them and return them. The model is refreshed first: collecting
    /// reads what the refresh indexes. A pending debounced recomputation
    /// becomes redundant and is dropped.
    fn recompute_diagnostics(&self, ws: &mut Workspace) -> Recomputed {
        let generation = self.diagnostics_generation.fetch_add(1, Ordering::SeqCst) + 1;
        #[cfg(test)]
        self.recalculations.fetch_add(1, Ordering::SeqCst);
        ws.refresh();
        let diagnostics = collect_diagnostics(ws.canonical_root(), ws);
        self.diagnostics.lock().clone_from(&diagnostics);
        Recomputed {
            generation,
            diagnostics,
            content_changed: std::mem::take(&mut *self.content_changed.lock()),
        }
    }

    /// Recompute and publish the diagnostics once [`DIAGNOSTICS_DEBOUNCE`]
    /// has passed, unless the diagnostics have moved past `generation` by
    /// then. The caller holds the workspace lock and has just advanced the
    /// generation to `generation`.
    fn schedule_diagnostics(&self, generation: u64) {
        let server = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(DIAGNOSTICS_DEBOUNCE).await;
            let diagnostics = {
                let mut guard = server.workspace.lock();
                let Some(ws) = guard.as_mut() else {
                    return;
                };
                if server.diagnostics_generation.load(Ordering::SeqCst) != generation {
                    return;
                }
                server.recompute_diagnostics(ws)
            };
            server.publish_workspace_diagnostics(&diagnostics).await;
        });
    }

    /// Publish the diagnostics of `recomputed` that the client lacks: of
    /// each file whose diagnostics changed since the last publication or
    /// whose text changed, and an empty list for each file that no longer
    /// has any. The other files are neither read nor converted.
    ///
    /// A recomputation older than the one last published is not published:
    /// its files with changed text are converted again from the newer
    /// diagnostics instead, which the newer publication may have skipped.
    ///
    /// Positions are converted against the text at publication time, not
    /// the text the diagnostics were computed from: an edit in between
    /// gives briefly wrong ranges, which the next quiet period corrects.
    async fn publish_workspace_diagnostics(&self, recomputed: &Recomputed) {
        let Recomputed {
            generation,
            diagnostics,
            content_changed,
        } = recomputed;
        let mut published = self.published.lock().await;
        let (changed, cleared): (Vec<_>, Vec<PathBuf>) = if *generation > published.generation {
            let changed = diagnostics
                .iter()
                .filter(|(path, file_diags)| {
                    content_changed.contains(*path)
                        || published.diagnostics.get(*path) != Some(file_diags)
                })
                .collect();
            let cleared = published
                .diagnostics
                .keys()
                .filter(|path| !diagnostics.contains_key(*path))
                .cloned()
                .collect();
            published.diagnostics.clone_from(diagnostics);
            published.generation = *generation;
            (changed, cleared)
        } else {
            let changed = content_changed
                .iter()
                .filter_map(|path| published.diagnostics.get_key_value(path))
                .collect();
            (changed, Vec::new())
        };

        // Convert without holding the documents lock across await points
        let conversions: Vec<(Uri, Vec<Diagnostic>)> = {
            let docs = self.documents.lock();
            changed
                .iter()
                .filter_map(|(path, file_diags)| {
                    let uri = symbols::path_to_uri(path)?;
                    let content: String = docs.get(&uri).map_or_else(
                        || asciidoc_core::source_text::read_adoc_file(path).unwrap_or_default(),
                        |s| (*s.content).clone(),
                    );
                    let lsp_diags = diagnostics_convert::convert_diagnostics(file_diags, &content);
                    Some((uri, lsp_diags))
                })
                .chain(
                    cleared
                        .iter()
                        .filter_map(|path| Some((symbols::path_to_uri(path)?, Vec::new()))),
                )
                .collect()
        };

        for (uri, lsp_diags) in conversions {
            #[cfg(test)]
            self.publications
                .lock()
                .push((uri.clone(), lsp_diags.clone()));
            self.client.publish_diagnostics(uri, lsp_diags, None).await;
        }
    }

    /// Scan `root` for all `.adoc` files in the background, then install the
    /// workspace and publish its diagnostics. Until then, notifications find
    /// no workspace and requests that read it wait.
    ///
    /// The scan runs on its own OS thread, not on `spawn_blocking`: dropping
    /// the runtime waits for blocking tasks, so `exit` would wait for the
    /// scan. The thread ends with the process.
    fn start_scan(&self, root: PathBuf) {
        self.workspace_ready.send_replace(false);
        *self.pending_changes.lock() = Some(Vec::new());
        let (scan_tx, scan_rx) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("workspace-scan".into())
            .spawn(move || {
                // tag::workspace-setup[]
                let mut ws = Workspace::new(&root);
                ws.scan_workspace();
                // end::workspace-setup[]
                // The receiver is gone only when the server has ended.
                scan_tx.send(ws).ok();
            });
        let thread = match thread {
            Ok(thread) => thread,
            Err(err) => {
                tracing::error!(?err, "cannot start the workspace scan");
                *self.pending_changes.lock() = None;
                self.workspace_ready.send_replace(true);
                return;
            }
        };
        #[cfg(test)]
        {
            *self.scan_thread.lock() = Some(thread);
        }
        #[cfg(not(test))]
        drop(thread);
        let server = self.clone();
        let install = tokio::spawn(async move {
            if let Ok(ws) = scan_rx.await {
                server.install_workspace(ws).await;
            } else {
                // The scan thread panicked: no install will take the queue.
                *server.pending_changes.lock() = None;
                server.workspace_ready.send_replace(true);
            }
        });
        #[cfg(test)]
        {
            *self.scan.lock() = Some(install);
        }
        #[cfg(not(test))]
        drop(install);
    }

    /// Make the scanned `ws` the server's workspace and publish its
    /// diagnostics. Buffers opened during the scan replace their disk text,
    /// as `did_open` does once there is a workspace; then the file changes
    /// reported during the scan are applied in order, except to the files
    /// open now, whose buffers win as they do over later watcher events.
    async fn install_workspace(&self, ws: Workspace) {
        let diagnostics = {
            let mut guard = self.workspace.lock();
            let ws = guard.insert(ws);
            // Unlike the other handlers, open buffers are read under the
            // workspace lock (order: workspace, then documents): `did_open`
            // stores its buffer before it takes the workspace lock, so a
            // buffer read before installing could miss one opened meanwhile.
            let open: Vec<Uri> = self.documents.lock().keys().cloned().collect();
            for uri in open {
                let Some(path) = uri.to_file_path().map(std::borrow::Cow::into_owned) else {
                    continue;
                };
                let Ok(canonical) = path.canonicalize() else {
                    continue;
                };
                let overrides = build_content_overrides(&self.documents, &uri);
                register_open_buffer(ws, &path, &canonical, &overrides);
            }
            let pending = self.pending_changes.lock().take().unwrap_or_default();
            let open_paths = self.open_buffer_paths();
            for mut changes in pending {
                changes
                    .adoc_events
                    .retain(|(path, _)| !is_path_open(path, &open_paths));
                let open = changes
                    .config_changed
                    .then(|| open_contents(&self.documents.lock()));
                // Nothing is published before the install: every file with
                // diagnostics is published, whatever its text.
                apply_classified_events(ws, changes, open.as_ref());
            }
            self.recompute_diagnostics(ws)
        };
        self.workspace_ready.send_replace(true);
        self.publish_workspace_diagnostics(&diagnostics).await;
    }

    /// Wait until no workspace scan runs: at once without one, else until its
    /// workspace is installed or the scan failed.
    async fn workspace_ready(&self) {
        // The sender lives in `self`, so the wait never ends with an error.
        self.workspace_ready
            .subscribe()
            .wait_for(|ready| *ready)
            .await
            .ok();
    }
}

// Handlers without `.await` stay `async fn`: the trait's signature, and the
// work runs when the future is polled, not when the handler is called.
#[allow(clippy::unused_async_trait_impl)]
impl LanguageServer for AsciiDocLsp {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        if let Some(pid) = params.process_id {
            watchdog::watch(pid);
        }

        // Capture client capability for dynamic watcher registration so we can
        // call `register_capability` in `initialized()` only when supported.
        {
            let supported = params
                .capabilities
                .workspace
                .as_ref()
                .and_then(|w| w.did_change_watched_files.as_ref())
                .and_then(|c| c.dynamic_registration)
                .unwrap_or(false);
            *self.can_request_file_watcher.lock() = supported;
            let show_document = params
                .capabilities
                .window
                .as_ref()
                .and_then(|w| w.show_document.as_ref())
                .is_some_and(|c| c.support);
            *self.can_show_document.lock() = show_document;
        }

        // As a server we read whichever workspace representation the client
        // sends — staying robust against older clients is cheap and avoids
        // "no workspace root" failures with non-mainstream LSP clients.
        // Try modern first, then deprecated fields:
        //   1. workspace_folders[0] (LSP ≥3.6, multi-root capable)
        //   2. root_uri (deprecated since 3.6, still common)
        //   3. root_path (legacy, deprecated since 3.0)
        #[allow(deprecated)]
        let root_path: Option<PathBuf> = params
            .workspace_folders
            .as_ref()
            .and_then(|folders| folders.first())
            .and_then(|folder| folder.uri.to_file_path().map(std::borrow::Cow::into_owned))
            .or_else(|| {
                params
                    .root_uri
                    .as_ref()
                    .and_then(|uri| uri.to_file_path().map(std::borrow::Cow::into_owned))
            })
            .or_else(|| params.root_path.as_ref().map(PathBuf::from));

        if let Some(root) = root_path {
            *self.root.lock() = Some(root);
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
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: semantic_tokens::legend(),
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: None,
                            work_done_progress_options: WorkDoneProgressOptions::default(),
                        },
                    ),
                ),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                })),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![
                            CodeActionKind::QUICKFIX,
                            CodeActionKind::REFACTOR_REWRITE,
                        ]),
                        ..Default::default()
                    },
                )),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        EXPLAIN_POSITION_COMMAND.to_string(),
                        PREVIEW_OPEN_COMMAND.to_string(),
                        PREVIEW_DEBUG_COMMAND.to_string(),
                        PREVIEW_TOC_COMMAND.to_string(),
                    ],
                    ..Default::default()
                }),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        "<".into(), // << xref
                        ":".into(), // include::
                        "{".into(), // {attribute
                        "/".into(), // next path segment
                        "[".into(), // [ block attribute line
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "asciidoc-lsp".into(),
                version: Some(version::build_version()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        // Start the scan before the first await, so a request that arrives
        // during the watcher registration already waits for the workspace.
        let root = self.root.lock().clone();
        if let Some(root) = root {
            self.start_scan(root);
        }

        // Register file watchers if the client supports dynamic registration.
        // Without this, external edits to `.asciidoctorconfig*`, `.adoc` and theme `.yml` files
        // outside open buffers leave caches stale.
        if *self.can_request_file_watcher.lock() {
            let registration = watched_files_registration();
            if let Err(err) = self.client.register_capability(vec![registration]).await {
                tracing::warn!(?err, "register_capability for didChangeWatchedFiles failed");
            }
        } else {
            tracing::info!(
                "client does not support dynamic registration of didChangeWatchedFiles; \
                 external file changes will not invalidate caches"
            );
        }
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let changed: Vec<PathBuf> = params
            .changes
            .iter()
            .filter_map(|event| document_path(&event.uri))
            .collect();
        // Before the events: a deleted file leaves the index and the read set.
        let read_before = self.preview_reads_any(&changed);
        if let Some(diagnostics) = self.compute_watched_files_update(params) {
            self.publish_workspace_diagnostics(&diagnostics).await;
        }
        // After them: a created file only enters the read set now.
        if read_before || self.preview_reads_any(&changed) {
            self.preview_files_changed();
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let content = asciidoc_core::source_text::strip_bom_owned(params.text_document.text);
        let version = params.text_document.version;

        self.documents.lock().insert(
            uri.clone(),
            DocumentState {
                content: Arc::new(content),
                version,
            },
        );

        // Per-file: ensure only this file, then recalculate diagnostics
        if let Some(path) = uri.to_file_path().map(std::borrow::Cow::into_owned) {
            if let Ok(canonical) = path.canonicalize() {
                let overrides = build_content_overrides(&self.documents, &uri);
                let diagnostics = {
                    let mut ws = self.workspace.lock();
                    if let Some(ws) = ws.as_mut() {
                        register_open_buffer(ws, &path, &canonical, &overrides);
                        self.content_changed.lock().insert(canonical.clone());
                        Some(self.recompute_diagnostics(ws))
                    } else {
                        None
                    }
                };
                if let Some(diagnostics) = diagnostics {
                    self.publish_workspace_diagnostics(&diagnostics).await;
                }
            }
        }
        self.preview_buffer_changed(&uri);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let new_version = params.text_document.version;

        // Update document store (lock scope limited)
        {
            let mut docs = self.documents.lock();
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
                        content: Arc::new(asciidoc_core::source_text::strip_bom_owned(change.text)),
                        version: new_version,
                    },
                );
            }
        }

        // Per-file: update only this file at once, so navigation and
        // completion see the edit; the diagnostics wait for a quiet period.
        // The workspace refresh (include targets, docdir) runs only with the
        // diagnostics, as it costs O(workspace): navigation into a freshly
        // typed include target can lag by up to the quiet period.
        if let Some(path) = uri.to_file_path().map(std::borrow::Cow::into_owned) {
            if let Ok(canonical) = path.canonicalize() {
                let overrides = build_content_overrides(&self.documents, &uri);
                let mut ws = self.workspace.lock();
                if let Some(ws) = ws.as_mut() {
                    tracing::debug!(
                        raw_path = ?path,
                        ?canonical,
                        "did_change: update file"
                    );
                    if let Some(dir) = path.parent() {
                        ws.set_doc_dir(&canonical, dir);
                    }
                    ws.update_file(&canonical, &overrides);
                    self.content_changed.lock().insert(canonical.clone());
                    let generation = self.diagnostics_generation.fetch_add(1, Ordering::SeqCst) + 1;
                    self.schedule_diagnostics(generation);
                }
            }
        }
        self.preview_buffer_changed(&uri);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.lock().remove(&uri);

        // Re-index file from disk so gd/gr still work for closed files
        if let Some(path) = uri.to_file_path().map(std::borrow::Cow::into_owned) {
            if let Ok(canonical) = path.canonicalize() {
                let diagnostics = {
                    let mut ws = self.workspace.lock();
                    if let Some(ws) = ws.as_mut() {
                        tracing::debug!(
                            raw_path = ?path,
                            ?canonical,
                            "did_close: re-index file from disk"
                        );
                        ws.update_file(&canonical, &HashMap::new());
                        self.content_changed.lock().insert(canonical.clone());
                        Some(self.recompute_diagnostics(ws))
                    } else {
                        self.defer_until_installed(ClassifiedEvents::changed_on_disk(path));
                        None
                    }
                };
                if let Some(diagnostics) = diagnostics {
                    self.publish_workspace_diagnostics(&diagnostics).await;
                }
            }
        }
        // The disk text replaces a discarded buffer in the preview.
        self.preview_buffer_changed(&uri);
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        // Re-parse from disk content (editor may have saved changes not sent via did_change)
        let uri = params.text_document.uri;
        if let Some(path) = uri.to_file_path().map(std::borrow::Cow::into_owned) {
            if let Ok(canonical) = path.canonicalize() {
                let overrides = build_content_overrides(&self.documents, &uri);
                let diagnostics = {
                    let mut ws = self.workspace.lock();
                    if let Some(ws) = ws.as_mut() {
                        tracing::debug!(raw_path = ?path, ?canonical, "did_save: update file");
                        if let Some(dir) = path.parent() {
                            ws.set_doc_dir(&canonical, dir);
                        }
                        ws.update_file(&canonical, &overrides);
                        Some(self.recompute_diagnostics(ws))
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

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let Some(content) = self.get_document_content(&params.text_document.uri) else {
            return Ok(None);
        };

        let data = semantic_tokens::semantic_tokens(&content).map_err(|e| {
            let mut error = jsonrpc::Error::internal_error();
            error.message = format!("semantic tokens: {e:#}").into();
            error
        })?;

        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let Some(content) = self.get_document_content(uri) else {
            tracing::debug!(?uri, "hover: no document content");
            return Ok(None);
        };

        let Some(doc_path) = document_path(uri) else {
            return Ok(None);
        };

        self.workspace_ready().await;
        let ws = self.workspace.lock();
        log_navigation_request("hover", &doc_path, position, &content, ws.is_some());
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
        self.workspace_ready().await;
        let ws = self.workspace.lock();
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

        let Some(doc_path) = document_path(uri) else {
            return Ok(None);
        };

        self.workspace_ready().await;
        let ws = self.workspace.lock();
        log_navigation_request(
            "goto_definition",
            &doc_path,
            position,
            &content,
            ws.is_some(),
        );
        let overrides = build_content_overrides(&self.documents, uri);
        let result = navigation::resolve_definition_locations(
            &content,
            position,
            &doc_path,
            ws.as_ref(),
            &overrides,
        );
        // Drop the lock before building response
        drop(ws);
        tracing::debug!(?result, "goto_definition: result");

        let client_uris = self.client_uris(uri);
        let mut locations: Vec<Location> = result
            .into_iter()
            .filter_map(|loc| {
                Some(Location {
                    uri: result_uri(&loc.path, &client_uris)?,
                    range: location_range(&loc, &overrides),
                })
            })
            .collect();

        // One target jumps directly; several let the client offer a choice.
        Ok(match locations.len() {
            0 => None,
            1 => locations.pop().map(GotoDefinitionResponse::Scalar),
            _ => Some(GotoDefinitionResponse::Array(locations)),
        })
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let Some(content) = self.get_document_content(uri) else {
            return Ok(None);
        };

        let Some(doc_path) = document_path(uri) else {
            return Ok(None);
        };

        let include_declaration = params.context.include_declaration;

        let overrides = build_content_overrides(&self.documents, uri);
        self.workspace_ready().await;
        let ws = self.workspace.lock();
        log_navigation_request("references", &doc_path, position, &content, ws.is_some());
        let results = navigation::resolve_references(
            &content,
            position,
            &doc_path,
            ws.as_ref(),
            include_declaration,
            &overrides,
        );
        drop(ws);
        tracing::debug!(
            count = results.as_ref().map(Vec::len),
            "references: result (None = no target)"
        );

        let Some(refs) = results else {
            return Ok(None);
        };

        let client_uris = self.client_uris(uri);
        let locations: Vec<Location> = refs
            .into_iter()
            .filter_map(|loc| {
                let target_uri = result_uri(&loc.path, &client_uris)?;
                Some(Location {
                    uri: target_uri,
                    range: location_range(&loc, &overrides),
                })
            })
            .collect();
        tracing::debug!(
            lsp_targets = ?locations
                .iter()
                .map(|loc| lsp_target(&loc.uri, &loc.range))
                .collect::<Vec<_>>(),
            "references: targets"
        );

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

        let Some(doc_path) = document_path(uri) else {
            return Ok(None);
        };

        self.workspace_ready().await;
        let ws = self.workspace.lock();
        log_navigation_request(
            "prepare_rename",
            &doc_path,
            position,
            &content,
            ws.is_some(),
        );
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

        let Some(doc_path) = document_path(uri) else {
            return Ok(None);
        };

        let overrides = build_content_overrides(&self.documents, uri);

        self.workspace_ready().await;
        let ws = self.workspace.lock();
        log_navigation_request("rename", &doc_path, position, &content, ws.is_some());
        if let Some(reason) =
            navigation::rename_conflict(&content, position, &doc_path, ws.as_ref(), &new_name)
        {
            return Err(jsonrpc::Error::invalid_params(reason));
        }
        let result =
            navigation::resolve_rename(&content, position, &doc_path, ws.as_ref(), &overrides);
        drop(ws);
        tracing::debug!(
            files = result.as_ref().map(HashMap::len),
            "rename: result (None = no target)"
        );

        let Some(file_edits) = result else {
            return Ok(None);
        };

        let client_uris = self.client_uris(uri);
        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
        for (path, edits) in file_edits {
            let Some(file_uri) = result_uri(&path, &client_uris) else {
                continue;
            };
            let file_content = overrides.get(&path).map_or_else(
                || {
                    asciidoc_core::source_text::read_adoc_file(&path).unwrap_or_else(|e| {
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
        tracing::debug!(
            lsp_targets = ?changes
                .iter()
                .flat_map(|(uri, edits)| edits.iter().map(|edit| lsp_target(uri, &edit.range)))
                .collect::<Vec<_>>(),
            "rename: targets"
        );

        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = &params.text_document.uri;
        let Some(content) = self.get_document_content(uri) else {
            return Ok(None);
        };

        self.workspace_ready().await;
        let mut actions = self.quick_fix_actions(uri, params.range, &content);

        let (line, _) = positions::from_lsp_position(params.range.start, &content);
        match asciidoc_core::ventilate::ventilate_paragraph(&content, line) {
            Ok(Some(edit)) => {
                let range = Range {
                    start: positions::offset_to_lsp_position(edit.range.start, &content),
                    end: positions::offset_to_lsp_position(edit.range.end, &content),
                };
                let changes = HashMap::from([(
                    uri.clone(),
                    vec![TextEdit {
                        range,
                        new_text: edit.new_text,
                    }],
                )]);
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: "One sentence per line".to_string(),
                    kind: Some(CodeActionKind::REFACTOR_REWRITE),
                    edit: Some(WorkspaceEdit {
                        changes: Some(changes),
                        ..Default::default()
                    }),
                    ..Default::default()
                }));
            }
            Ok(None) => {}
            Err(err) => tracing::warn!(?err, "ventilate_paragraph failed"),
        }

        Ok((!actions.is_empty()).then_some(actions))
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>> {
        if params.command == PREVIEW_OPEN_COMMAND {
            return self.preview_open(params.arguments).await;
        }
        if params.command == PREVIEW_DEBUG_COMMAND {
            return self.preview_debug(&params.arguments);
        }
        if params.command == PREVIEW_TOC_COMMAND {
            return self.preview_toc(&params.arguments);
        }
        let args = parse_explain_position_command(&params.command, params.arguments)?;
        let uri = &args.text_document.uri;

        let Some(content) = self.get_document_content(uri) else {
            return Err(jsonrpc::Error::invalid_params(format!(
                "{EXPLAIN_POSITION_COMMAND}: no content for {}",
                uri.as_str()
            )));
        };
        let (Some(raw_path), Some(doc_path)) = (uri.to_file_path(), document_path(uri)) else {
            return Err(jsonrpc::Error::invalid_params(format!(
                "{EXPLAIN_POSITION_COMMAND}: not a file URI: {}",
                uri.as_str()
            )));
        };

        self.workspace_ready().await;
        let ws = self.workspace.lock();
        Ok(Some(navigation::explain_position(
            &content,
            args.position,
            &raw_path,
            &doc_path,
            ws.as_ref(),
        )))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let Some(content) = self.get_document_content(uri) else {
            return Ok(None);
        };

        // Detect completion context from cursor position
        let Some(ctx) = completion::detect_completion_context(&content, position)
            .or_else(|| completion::detect_attribute_value_path(&content, position))
        else {
            return Ok(None);
        };

        let Some(doc_path) = document_path(uri) else {
            return Ok(None);
        };

        self.workspace_ready().await;
        let ws = self.workspace.lock();
        let line = position.line as usize + 1;
        let items = completion::resolve_completions(ctx, &doc_path, line, ws.as_ref());

        if items.is_empty() {
            Ok(None)
        } else {
            Ok(Some(CompletionResponse::Array(items)))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchedKind {
    Config,
    AsciiDoc,
    Image,
    /// A YAML file that may be a PDF theme.
    Theme,
}

/// Extensions of the image files whose appearance or removal changes the
/// missing-image diagnostics.
const IMAGE_EXTENSIONS: [&str; 6] = ["png", "jpg", "jpeg", "gif", "svg", "webp"];

/// One watcher per image extension.
fn image_watchers() -> impl Iterator<Item = FileSystemWatcher> {
    IMAGE_EXTENSIONS.into_iter().map(|ext| FileSystemWatcher {
        glob_pattern: GlobPattern::String(format!("**/*.{ext}")),
        kind: None,
    })
}

/// The `workspace/didChangeWatchedFiles` registration sent from `initialized`:
/// config files, `.adoc` sources, theme `.yml` files and image files.
fn watched_files_registration() -> Registration {
    let watchers = vec![
        FileSystemWatcher {
            glob_pattern: GlobPattern::String("**/.asciidoctorconfig".into()),
            kind: None,
        },
        FileSystemWatcher {
            glob_pattern: GlobPattern::String("**/.asciidoctorconfig.adoc".into()),
            kind: None,
        },
        FileSystemWatcher {
            glob_pattern: GlobPattern::String("**/*.adoc".into()),
            kind: None,
        },
        FileSystemWatcher {
            glob_pattern: GlobPattern::String("**/*.asciidoc".into()),
            kind: None,
        },
        FileSystemWatcher {
            glob_pattern: GlobPattern::String("**/*.yml".into()),
            kind: None,
        },
    ];
    // Serializing a struct of `Vec<FileSystemWatcher>` cannot fail in
    // practice (no maps with non-string keys, no exotic types).
    let watchers = watchers.into_iter().chain(image_watchers()).collect();
    let register_options =
        serde_json::to_value(DidChangeWatchedFilesRegistrationOptions { watchers })
            .expect("DidChangeWatchedFilesRegistrationOptions serialization");
    Registration {
        id: "asciidoc-lsp/watched-files".into(),
        method: "workspace/didChangeWatchedFiles".into(),
        register_options: Some(register_options),
    }
}

/// AsciiDoc-relevant events extracted from a `DidChangeWatchedFilesParams`
/// batch, with `.adoc` events for open buffers filtered out (LSP text-sync
/// owns those).
#[derive(Debug, Default)]
struct ClassifiedEvents {
    config_changed: bool,
    /// An image file appeared, vanished or changed: the diagnostics need a
    /// recalculation, no document is parsed again.
    images_changed: bool,
    theme_changed: bool,
    adoc_events: Vec<(PathBuf, FileChangeType)>,
}

impl ClassifiedEvents {
    /// `path` changed on disk: what `didClose` reports.
    fn changed_on_disk(path: PathBuf) -> Self {
        Self {
            adoc_events: vec![(path, FileChangeType::CHANGED)],
            ..Self::default()
        }
    }
}

/// Apply watcher events to `ws` without recomputing its diagnostics. `open`
/// holds the open buffers' content when the config changed. Returns the
/// files parsed again from disk.
fn apply_classified_events(
    ws: &mut Workspace,
    classified: ClassifiedEvents,
    open: Option<&HashMap<PathBuf, Arc<String>>>,
) -> Vec<PathBuf> {
    if let Some(open) = open {
        ws.reload_config(open);
    }

    // CREATED/CHANGED re-parse from disk (no overrides — open buffers were
    // filtered upstream). DELETED drops the file from all indexes.
    let overrides = HashMap::new();
    let mut reparsed = Vec::new();
    for (path, typ) in classified.adoc_events {
        if typ == FileChangeType::DELETED {
            ws.remove_file(&resolve_deleted_doc_key(&path));
        } else if typ == FileChangeType::CREATED || typ == FileChangeType::CHANGED {
            let Ok(canonical) = path.canonicalize() else {
                continue;
            };
            let indexed = ws.documents().contains_key(&canonical);
            tracing::debug!(
                raw_path = ?path,
                ?canonical,
                indexed,
                "did_change_watched_files: register file"
            );
            if indexed {
                ws.update_file(&canonical, &overrides);
            } else {
                ws.ensure_file(&canonical, &overrides);
            }
            reparsed.push(canonical);
        }
        // Other (server-side proposed) FileChangeType values: ignored.
    }
    reparsed
}

/// Filter and bucket a watcher-event batch.
/// Drops `.adoc` events for open buffers and events for unrelated paths.
/// Returns `None` when nothing relevant remains, so the caller can skip the
/// workspace lock and publish step entirely.
fn classify_events(
    changes: Vec<FileEvent>,
    open_buffers: &HashSet<PathBuf>,
) -> Option<ClassifiedEvents> {
    let mut out = ClassifiedEvents::default();
    for change in changes {
        let Some(path) = change.uri.to_file_path() else {
            continue;
        };
        let path: PathBuf = path.into_owned();
        match classify_watched_path(&path) {
            // Config chains are read from disk, open in the editor or not.
            Some(WatchedKind::Config) => out.config_changed = true,
            // Theme files are read from disk on every recalculation.
            Some(WatchedKind::Theme) => out.theme_changed = true,
            Some(WatchedKind::AsciiDoc) if !is_path_open(&path, open_buffers) => {
                out.adoc_events.push((path, change.typ));
            }
            Some(WatchedKind::Image) => out.images_changed = true,
            Some(WatchedKind::AsciiDoc) | None => {}
        }
    }
    if !out.config_changed
        && !out.images_changed
        && !out.theme_changed
        && out.adoc_events.is_empty()
    {
        None
    } else {
        Some(out)
    }
}

/// Whether `path` (from a watcher event) refers to a path the editor owns.
/// `open_buffers` should be a snapshot from `open_buffer_paths()` containing
/// both the URI-derived form and the canonicalized form of every open buffer.
/// Tries an additional canonicalize on `path` itself to bridge symlink and
/// non-canonical inputs from the client.
fn is_path_open(path: &Path, open_buffers: &HashSet<PathBuf>) -> bool {
    open_buffers.contains(path) || path.canonicalize().is_ok_and(|c| open_buffers.contains(&c))
}

/// Log a navigation request at debug level: handler, raw document path,
/// position and the node kind found there. The node lookup re-parses the
/// document, so it only runs when debug logging is enabled.
fn log_navigation_request(
    handler: &'static str,
    doc_path: &Path,
    position: Position,
    content: &str,
    workspace_exists: bool,
) {
    if tracing::enabled!(tracing::Level::DEBUG) {
        let node = navigation::find_node_at_position(content, position).map(|n| n.kind);
        tracing::debug!(
            handler,
            ?doc_path,
            lsp_line = position.line,
            lsp_character = position.character,
            ?node,
            workspace_exists,
            "navigation request"
        );
    }
}

/// Format a navigation target for the debug log in the LSP convention of
/// the request log: `uri:line:character-end_line:end_character`, lines
/// 0-based, characters in UTF-16 code units.
fn lsp_target(uri: &Uri, range: &Range) -> String {
    format!(
        "{}:{}:{}-{}:{}",
        uri.as_str(),
        range.start.line,
        range.start.character,
        range.end.line,
        range.end.character
    )
}

/// Resolve a `documents`-key for a path that has just been deleted.
/// The deleted file no longer exists, but its parent directory typically does;
/// canonicalizing the parent and re-attaching the filename produces the same
/// key that `ensure_file()` / `update_file()` originally inserted. Falls back
/// to the unmodified path when the parent is also gone.
fn resolve_deleted_doc_key(path: &Path) -> PathBuf {
    path.parent()
        .and_then(|parent| parent.canonicalize().ok())
        .and_then(|canonical_parent| path.file_name().map(|name| canonical_parent.join(name)))
        .unwrap_or_else(|| path.to_path_buf())
}

/// Classify a watched-file path. The watcher globs may match unrelated files
/// (`**/*` is permissive); the handler routes only `.asciidoctorconfig*`,
/// `.adoc` / `.asciidoc`, `.yml` and image files, and ignores everything else.
fn classify_watched_path(path: &Path) -> Option<WatchedKind> {
    let name = path.file_name()?.to_str()?;
    let ext = path.extension().and_then(|e| e.to_str());
    match (name, ext) {
        (".asciidoctorconfig" | ".asciidoctorconfig.adoc", _) => Some(WatchedKind::Config),
        (_, Some("adoc" | "asciidoc")) => Some(WatchedKind::AsciiDoc),
        (_, Some("yml")) => Some(WatchedKind::Theme),
        (_, Some(ext)) if IMAGE_EXTENSIONS.iter().any(|i| i.eq_ignore_ascii_case(ext)) => {
            Some(WatchedKind::Image)
        }
        _ => None,
    }
}

/// Command name of the position debug report.
const EXPLAIN_POSITION_COMMAND: &str = "asciidoc.explainPosition";

/// Command that opens the preview: the URI of the active document and
/// optionally `{"mode": "document" | "part"}` (default `document`: the
/// top-level document that includes it; `part`: the document alone). Answers
/// `{"url": "<page URL>"}`.
const PREVIEW_OPEN_COMMAND: &str = "asciidoc.preview.open";

/// Command for the preview's scroll-sync debug log: `"on"`, `"off"` or
/// `"show"`. Fails while no preview is open.
const PREVIEW_DEBUG_COMMAND: &str = "asciidoc.preview.debug";

/// Command that sets the preview's TOC: `"auto"`, `"off"`, `"left"` or
/// `"right"`. Answers `null`; fails while no preview is open.
const PREVIEW_TOC_COMMAND: &str = "asciidoc.preview.toc";

/// Notification from the editor: its cursor moved. Params
/// [`CursorMovedParams`].
const CURSOR_MOVED_METHOD: &str = "asciidoc/cursorMoved";

/// Params of [`CURSOR_MOVED_METHOD`].
#[derive(Debug, serde::Deserialize)]
struct CursorMovedParams {
    uri: Uri,
    /// 0-based, like an LSP position. The preview server turns it into the
    /// 1-based line of Asciidoctor's `data-line-N`.
    line: u32,
}

/// Notification to the editor: the reader scrolled the preview, show this
/// line. Params [`RevealLineParams`]. Sent for the active document only.
enum RevealLine {}

impl tower_lsp_server::ls_types::notification::Notification for RevealLine {
    type Params = RevealLineParams;
    const METHOD: &'static str = "asciidoc/revealLine";
}

/// Params of [`RevealLine`].
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct RevealLineParams {
    uri: Uri,
    /// 0-based, like an LSP position; past the end means the last line.
    line: u32,
    /// Move the cursor to `line` as well, not only the view.
    cursor: bool,
}

/// What the editor gets for a page action.
#[derive(Debug, PartialEq)]
enum EditorRequest {
    ShowDocument(ShowDocumentParams),
    RevealLine(RevealLineParams),
}

/// The request for a page action, or `None` if there is nothing to send.
///
/// A click opens its file at its line, also another file than the active
/// one, by the client's URI for an open buffer (`client_uris`), and only if
/// the client handles `window/showDocument`. A scroll reveals its line only
/// in the `active` document: it never switches the file. The page's lines
/// are 1-based source lines; LSP lines are 0-based.
fn editor_request(
    action: preview::PageAction,
    active: Option<&Uri>,
    client_uris: &HashMap<PathBuf, Uri>,
    can_show_document: bool,
) -> Option<EditorRequest> {
    match action {
        preview::PageAction::Click { file, line } => {
            if !can_show_document {
                return None;
            }
            let start = Position::new(line.saturating_sub(1), 0);
            Some(EditorRequest::ShowDocument(ShowDocumentParams {
                uri: result_uri(&file, client_uris)?,
                external: None,
                take_focus: Some(true),
                selection: Some(Range::new(start, start)),
            }))
        }
        // The LSP handles it itself, see `forward_page_actions`.
        preview::PageAction::Toc(_) => None,
        preview::PageAction::Scroll { file, line, cursor } => {
            let active = active?;
            (document_path(active)? == file).then(|| {
                EditorRequest::RevealLine(RevealLineParams {
                    uri: active.clone(),
                    line: line.saturating_sub(1),
                    cursor,
                })
            })
        }
    }
}

/// A preview failure as a JSON-RPC error with the cause as message.
fn preview_error(error: impl std::fmt::Display) -> jsonrpc::Error {
    let mut err = jsonrpc::Error::internal_error();
    err.message = format!("AsciiDoc preview: {error}").into();
    err
}

/// Validate a `workspace/executeCommand` request for
/// [`EXPLAIN_POSITION_COMMAND`]: the command must match and `arguments` must
/// hold exactly one object shaped like `TextDocumentPositionParams`.
/// Anything else is an `InvalidParams` error.
fn parse_explain_position_command(
    command: &str,
    arguments: Vec<serde_json::Value>,
) -> Result<TextDocumentPositionParams> {
    if command != EXPLAIN_POSITION_COMMAND {
        return Err(jsonrpc::Error::invalid_params(format!(
            "unknown command: {command}"
        )));
    }
    let [argument]: [serde_json::Value; 1] = arguments.try_into().map_err(|_| {
        jsonrpc::Error::invalid_params(format!(
            "{EXPLAIN_POSITION_COMMAND} expects exactly one argument \
             {{textDocument: {{uri}}, position: {{line, character}}}}"
        ))
    })?;
    serde_json::from_value(argument).map_err(|err| {
        jsonrpc::Error::invalid_params(format!(
            "{EXPLAIN_POSITION_COMMAND}: invalid argument: {err}"
        ))
    })
}

/// The path a document is registered under in the workspace: canonical when
/// `canonicalize` succeeds, the raw URI path otherwise (unsaved new file).
fn document_path(uri: &Uri) -> Option<PathBuf> {
    let raw = uri.to_file_path()?;
    Some(raw.canonicalize().unwrap_or_else(|_| raw.into_owned()))
}

/// URI of a result path: the client's own URI for that document where it has
/// one, so a result does not address a second buffer of an open file.
fn result_uri(path: &Path, client_uris: &HashMap<PathBuf, Uri>) -> Option<Uri> {
    client_uris
        .get(path)
        .cloned()
        .or_else(|| Uri::from_file_path(path))
}

/// The LSP range of a byte-column span, converted to UTF-16 against the
/// file's content: an open buffer's if there is one, else the disk's.
fn location_range(
    loc: &asciidoc_core::types::FileLocation,
    overrides: &HashMap<PathBuf, Arc<String>>,
) -> Range {
    let content = overrides.get(&loc.path).map_or_else(
        || asciidoc_core::source_text::read_adoc_file(&loc.path).unwrap_or_default(),
        |content| (**content).clone(),
    );
    Range {
        start: positions::to_lsp_position(loc.line, loc.col, &content),
        end: positions::to_lsp_position(loc.line, loc.end_col, &content),
    }
}

/// Index the open buffer at `path` (canonical: `canonical`) with its editor
/// content from `overrides`. A file already indexed, e.g. by the scan, is
/// re-parsed so the editor content replaces the disk content.
fn register_open_buffer(
    ws: &mut Workspace,
    path: &Path,
    canonical: &Path,
    overrides: &HashMap<PathBuf, Arc<String>>,
) {
    let indexed = ws.documents().contains_key(canonical);
    tracing::debug!(raw_path = ?path, ?canonical, indexed, "register open buffer");
    if let Some(dir) = path.parent() {
        ws.set_doc_dir(canonical, dir);
    }
    if indexed {
        ws.update_file(canonical, overrides);
    } else {
        ws.ensure_file(canonical, overrides);
    }
}

/// Build a `HashMap` of file path -> content for all open documents.
/// Used as overrides so the semantic layer reads in-memory content
/// instead of stale disk content. Keys are [`document_path`]s, the form the
/// workspace reads files under. Two buffers of one file (raw and canonical
/// URI) share a key; the content of `active` wins.
fn build_content_overrides(
    documents: &Mutex<HashMap<Uri, DocumentState>>,
    active: &Uri,
) -> HashMap<PathBuf, Arc<String>> {
    let docs = documents.lock();
    let mut overrides = open_contents(&docs);
    if let (Some(path), Some(state)) = (document_path(active), docs.get(active)) {
        overrides.insert(path, Arc::clone(&state.content));
    }
    overrides
}

/// Content of every open document keyed by [`document_path`]. Of two
/// buffers of one file, either one's content is taken.
fn open_contents(docs: &HashMap<Uri, DocumentState>) -> HashMap<PathBuf, Arc<String>> {
    docs.iter()
        .filter_map(|(uri, state)| Some((document_path(uri)?, Arc::clone(&state.content))))
        .collect()
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if version::wants_version(std::env::args().skip(1)) {
        println!("{}", version::version_line(&version::build_version()));
        return Ok(());
    }

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

    let (service, socket) = LspService::build(AsciiDocLsp::new)
        .custom_method(CURSOR_MOVED_METHOD, AsciiDocLsp::cursor_moved)
        .finish();
    Server::new(stdin, stdout, socket).serve(service).await;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{all_diagnostics, diagnostics_map};
    use std::fs;
    use std::path::Path;
    use std::str::FromStr;
    use tower_lsp_server::jsonrpc::ErrorCode;
    use tower_lsp_server::ls_types::{
        PartialResultParams, SemanticTokenType, TextDocumentIdentifier, WorkDoneProgressParams,
    };

    // --- Document Lifecycle tests ---

    #[test]
    fn test_did_open_stores_document() {
        let docs: Mutex<HashMap<Uri, DocumentState>> = Mutex::new(HashMap::new());
        let uri = Uri::from_str("file:///test.adoc").unwrap();
        let content = "= Title\n\nContent.".to_string();

        docs.lock().insert(
            uri.clone(),
            DocumentState {
                content: Arc::new(content.clone()),
                version: 1,
            },
        );

        let stored = docs.lock();
        let state = stored.get(&uri).unwrap();
        assert_eq!(*state.content, content);
        assert_eq!(state.version, 1);
    }

    #[test]
    fn test_did_change_updates_content() {
        let docs: Mutex<HashMap<Uri, DocumentState>> = Mutex::new(HashMap::new());
        let uri = Uri::from_str("file:///test.adoc").unwrap();

        // Open
        docs.lock().insert(
            uri.clone(),
            DocumentState {
                content: Arc::new("old content".into()),
                version: 1,
            },
        );

        // Change with newer version
        {
            let mut locked = docs.lock();
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

        let stored = docs.lock();
        let state = stored.get(&uri).unwrap();
        assert_eq!(*state.content, "new content");
        assert_eq!(state.version, 2);
    }

    #[test]
    fn test_did_change_ignores_old_version() {
        let docs: Mutex<HashMap<Uri, DocumentState>> = Mutex::new(HashMap::new());
        let uri = Uri::from_str("file:///test.adoc").unwrap();

        // Open with version 5
        docs.lock().insert(
            uri.clone(),
            DocumentState {
                content: Arc::new("current content".into()),
                version: 5,
            },
        );

        // Attempt change with older version (simulates out-of-order delivery)
        {
            let mut locked = docs.lock();
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

        let stored = docs.lock();
        let state = stored.get(&uri).unwrap();
        assert_eq!(*state.content, "current content");
        assert_eq!(state.version, 5);
    }

    #[test]
    fn test_did_close_removes_document() {
        let docs: Mutex<HashMap<Uri, DocumentState>> = Mutex::new(HashMap::new());
        let uri = Uri::from_str("file:///test.adoc").unwrap();

        docs.lock().insert(
            uri.clone(),
            DocumentState {
                content: Arc::new("content".into()),
                version: 1,
            },
        );
        assert!(docs.lock().contains_key(&uri));

        docs.lock().remove(&uri);
        assert!(!docs.lock().contains_key(&uri));
    }

    // --- Content Override tests ---

    #[test]
    fn test_build_content_overrides() {
        let docs: Mutex<HashMap<Uri, DocumentState>> = Mutex::new(HashMap::new());
        let uri = Uri::from_str("file:///tmp/test.adoc").unwrap();

        docs.lock().insert(
            uri.clone(),
            DocumentState {
                content: Arc::new("overridden content".into()),
                version: 1,
            },
        );

        let overrides = build_content_overrides(&docs, &uri);
        assert_eq!(overrides.len(), 1);
        assert_eq!(
            overrides.get(Path::new("/tmp/test.adoc")).unwrap().as_str(),
            "overridden content"
        );
    }

    #[test]
    fn test_build_content_overrides_empty() {
        let docs: Mutex<HashMap<Uri, DocumentState>> = Mutex::new(HashMap::new());
        let uri = Uri::from_str("file:///tmp/test.adoc").unwrap();
        let overrides = build_content_overrides(&docs, &uri);
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
        let b_doc = ws.documents().get(&b_canon).unwrap();
        assert_eq!(b_doc.sections[0].title, "Modified Chapter B");
    }

    // -----------------------------------------------------------------------
    // Integration Tests — arc42 Fixture (AC-12)
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
            ws.documents().len() >= 6,
            "workspace should have at least 6 documents (arc42 has ~10), found: {}",
            ws.documents().len()
        );

        // The entry point file should be in the documents
        let entry_canon = entry.canonicalize().unwrap();
        assert!(
            ws.documents().contains_key(&entry_canon),
            "workspace should contain entry file"
        );

        // The ID catalogs should define anchors from the fixture
        assert!(
            !ws.anchor_definitions().is_empty(),
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
        let all_diags = all_diagnostics(&ws);
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

        let targets = navigation::resolve_goto_definitions(&content, pos, &index, Some(&ws));
        assert_eq!(
            targets.len(),
            1,
            "goto-definition on include should resolve to one target, got: {targets:?}"
        );

        let (target, line) = targets.into_iter().next().unwrap();
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
        let file = docs_dir.join("chapters/subsections/workspace_manager.adoc");
        let content = fs::read_to_string(&file).unwrap();

        let ws_root = workspace_root();
        let ws = Workspace::build(&ws_root, &index);

        // Line 11 in workspace_manager.adoc:
        // include::{lsp-source-dir}/main.rs[tags=workspace-setup]
        // {lsp-source-dir} is at columns 9..25 (0-indexed LSP line 10).
        let pos = Position {
            line: 10,
            character: 14, // inside "lsp-source-dir"
        };

        let targets = navigation::resolve_goto_definitions(&content, pos, &file, Some(&ws));
        assert_eq!(
            targets.len(),
            1,
            "goto-definition on {{lsp-source-dir}} should resolve to .asciidoctorconfig, got: {targets:?}"
        );

        let (target, line) = targets.into_iter().next().unwrap();
        assert!(
            target.to_string_lossy().contains(".asciidoctorconfig"),
            "target should be .asciidoctorconfig, got: {:?}",
            target
        );
        // Verify the line actually contains the cli-source-dir attribute definition
        let config_path = target;
        let config_content = std::fs::read_to_string(&config_path).unwrap();
        let config_line = config_content.lines().nth(line - 1).unwrap_or("");
        assert!(
            config_line.contains(":lsp-source-dir:"),
            "line {} of .asciidoctorconfig should contain :lsp-source-dir:, got: {:?}",
            line,
            config_line
        );
    }

    // --- Workspace lifecycle tests (Live Diagnostics) ---

    #[test]
    fn test_new_workspace_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Workspace::new creates an empty workspace — files added via ensure_file
        let ws = Workspace::new(root);
        assert!(ws.documents().is_empty(), "Rebuild returns empty workspace");
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
        let doc = ws.documents().get(&index_canon).unwrap();
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
        assert!(
            ws.documents().is_empty(),
            "Empty workspace has no documents"
        );
        assert!(
            diagnostics_map(&ws).is_empty(),
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
        assert!(ws.documents().is_empty(), "Rebuild starts empty");

        // Simulate did_open: ensure standalone is added
        let standalone_canon = standalone.canonicalize().unwrap();
        ws.ensure_file(&standalone_canon, &overrides);
        assert_eq!(ws.documents().len(), 1, "Standalone added");

        // Simulate rebuild (e.g. after did_change on another file)
        let mut ws2 = Workspace::new(root);
        assert!(ws2.documents().is_empty(), "Rebuild starts empty again");

        // Re-ensure standalone after workspace recreation
        ws2.ensure_file(&standalone_canon, &overrides);
        assert_eq!(
            ws2.documents().len(),
            1,
            "Standalone preserved after rebuild"
        );
        assert!(ws2.documents().contains_key(&standalone_canon));
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

        assert_eq!(ws.documents().len(), 1);
        let doc = ws.documents().get(&standalone_canon).unwrap();
        assert_eq!(doc.attributes.len(), 1);
        assert_eq!(doc.attribute_refs.len(), 1);

        // Config chain should be loaded
        let scopes = ws.scopes_at(&standalone_canon, 4);
        assert_eq!(scopes[0].value("myattr").as_deref(), Some("val"));
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

    // --- Workspace-Root from LSP root_uri ---

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
            ws.root().to_path_buf(),
            sub.canonicalize().unwrap(),
            "Workspace root must be the LSP root_uri, not walked up to .asciidoctorconfig"
        );
    }

    // --- watched-file classification + buffer filter ---

    #[test]
    fn test_classify_watched_path_config() {
        assert_eq!(
            classify_watched_path(Path::new("/ws/.asciidoctorconfig")),
            Some(WatchedKind::Config)
        );
        assert_eq!(
            classify_watched_path(Path::new("/ws/sub/.asciidoctorconfig.adoc")),
            Some(WatchedKind::Config)
        );
    }

    #[test]
    fn test_classify_watched_path_adoc() {
        assert_eq!(
            classify_watched_path(Path::new("/ws/foo.adoc")),
            Some(WatchedKind::AsciiDoc)
        );
        assert_eq!(
            classify_watched_path(Path::new("/ws/sub/bar.asciidoc")),
            Some(WatchedKind::AsciiDoc)
        );
    }

    #[test]
    fn test_classify_watched_path_image() {
        for name in [
            "a.png", "a.jpg", "a.jpeg", "a.gif", "a.svg", "a.webp", "A.PNG",
        ] {
            assert_eq!(
                classify_watched_path(&Path::new("/ws/img").join(name)),
                Some(WatchedKind::Image),
                "{name}"
            );
        }
    }

    #[test]
    fn test_classify_events_image_only_batch_is_relevant() {
        let open: HashSet<PathBuf> = HashSet::new();
        let changes = vec![make_event(
            "file:///ws/img/logo.png",
            FileChangeType::CREATED,
        )];
        let classified = classify_events(changes, &open).expect("image event is relevant");
        assert!(classified.images_changed);
        assert!(!classified.config_changed);
        assert!(classified.adoc_events.is_empty());
    }

    #[test]
    fn test_image_watchers_cover_image_extensions() {
        let globs: Vec<String> = image_watchers()
            .filter_map(|w| match w.glob_pattern {
                GlobPattern::String(glob) => Some(glob),
                GlobPattern::Relative(_) => None,
            })
            .collect();
        assert_eq!(globs.len(), IMAGE_EXTENSIONS.len());
        assert!(globs.contains(&"**/*.png".to_string()));
        assert!(globs.contains(&"**/*.svg".to_string()));
    }

    #[test]
    fn test_watched_files_registration_includes_image_globs() {
        let registration = watched_files_registration();
        assert_eq!(registration.method, "workspace/didChangeWatchedFiles");
        let options: DidChangeWatchedFilesRegistrationOptions =
            serde_json::from_value(registration.register_options.expect("register options"))
                .expect("deserialize registration options");
        let globs: Vec<String> = options
            .watchers
            .into_iter()
            .filter_map(|w| match w.glob_pattern {
                GlobPattern::String(glob) => Some(glob),
                GlobPattern::Relative(_) => None,
            })
            .collect();
        assert!(globs.contains(&"**/*.png".to_string()));
        assert!(globs.contains(&"**/*.adoc".to_string()));
    }

    #[test]
    fn test_classify_watched_path_unrelated() {
        assert!(classify_watched_path(Path::new("/ws/README.md")).is_none());
        assert!(classify_watched_path(Path::new("/ws/.gitignore")).is_none());
        // .adoc extension takes precedence; .asciidoctorconfig.adoc is config (special name).
        assert_eq!(
            classify_watched_path(Path::new("/ws/notes.adoc")),
            Some(WatchedKind::AsciiDoc)
        );
    }

    #[test]
    fn test_classify_watched_path_theme_yaml() {
        assert_eq!(
            classify_watched_path(Path::new("/ws/themes/custom-theme.yml")),
            Some(WatchedKind::Theme)
        );
        assert!(classify_watched_path(Path::new("/ws/other.yaml")).is_none());
    }

    // --- open-buffer filter + deleted-key resolution ---

    #[test]
    fn test_is_path_open_direct_match() {
        let mut set: HashSet<PathBuf> = HashSet::new();
        set.insert(PathBuf::from("/ws/foo.adoc"));
        assert!(is_path_open(Path::new("/ws/foo.adoc"), &set));
        assert!(!is_path_open(Path::new("/ws/bar.adoc"), &set));
    }

    #[cfg(unix)]
    #[test]
    fn test_is_path_open_through_symlink() {
        // Watcher reports canonical path; editor opened the symlink. The set
        // built from `documents` keys must contain both forms so the filter
        // catches the canonical-path event.
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().canonicalize().unwrap().join("real.adoc");
        fs::write(&real, "= Real\n").unwrap();
        let link = tmp.path().canonicalize().unwrap().join("link.adoc");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // Editor opened `link.adoc`; `open_buffer_paths()` includes the
        // canonical resolution as well.
        let mut open_buffers: HashSet<PathBuf> = HashSet::new();
        open_buffers.insert(link.clone());
        if let Ok(canonical) = link.canonicalize() {
            open_buffers.insert(canonical);
        }

        // Watcher fires with canonical path → filter must match.
        assert!(is_path_open(&real, &open_buffers));
        // Watcher fires with the symlink path → filter must also match.
        assert!(is_path_open(&link, &open_buffers));
    }

    #[test]
    fn test_resolve_deleted_doc_key_canonicalizes_parent() {
        // The deleted file no longer exists, but its parent does. The key we
        // produce must equal what `ensure_file()` originally inserted (which
        // canonicalizes the full path before insert).
        let tmp = tempfile::tempdir().unwrap();
        let canonical_dir = tmp.path().canonicalize().unwrap();
        let deleted = canonical_dir.join("ghost.adoc");
        // Don't create the file — simulate it was already removed.

        let key = resolve_deleted_doc_key(&deleted);
        assert_eq!(key, canonical_dir.join("ghost.adoc"));
    }

    #[test]
    fn test_resolve_deleted_doc_key_disambiguates_same_filename() {
        // Two files with the same name in different dirs. Component-wise
        // `ends_with` matching would conflate them; canonicalized parent
        // resolution must keep them apart.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let dir_a = root.join("a");
        let dir_b = root.join("b");
        fs::create_dir(&dir_a).unwrap();
        fs::create_dir(&dir_b).unwrap();

        let key_a = resolve_deleted_doc_key(&dir_a.join("page.adoc"));
        let key_b = resolve_deleted_doc_key(&dir_b.join("page.adoc"));

        assert_ne!(key_a, key_b);
        assert!(key_a.starts_with(&dir_a));
        assert!(key_b.starts_with(&dir_b));
    }

    #[test]
    fn test_resolve_deleted_doc_key_falls_back_when_parent_gone() {
        // If even the parent dir is gone, return the path unchanged so the
        // caller can still attempt a `remove_file()` against indexes (no-op
        // if the key was canonical and we're not).
        let path = Path::new("/nonexistent/parent/file.adoc");
        let key = resolve_deleted_doc_key(path);
        assert_eq!(key, path);
    }

    // --- classify_events end-to-end ---

    fn make_event(uri: &str, typ: FileChangeType) -> FileEvent {
        FileEvent {
            uri: Uri::from_str(uri).unwrap(),
            typ,
        }
    }

    #[test]
    fn test_classify_events_buckets_config_and_adoc() {
        let open: HashSet<PathBuf> = HashSet::new();
        let changes = vec![
            make_event("file:///ws/.asciidoctorconfig", FileChangeType::CHANGED),
            make_event("file:///ws/foo.adoc", FileChangeType::CREATED),
            make_event("file:///ws/bar.adoc", FileChangeType::DELETED),
            make_event("file:///ws/README.md", FileChangeType::CHANGED),
        ];
        let classified = classify_events(changes, &open).expect("relevant events present");
        assert!(classified.config_changed);
        assert_eq!(classified.adoc_events.len(), 2);
        let kinds: Vec<_> = classified.adoc_events.iter().map(|(_, t)| *t).collect();
        assert!(kinds.contains(&FileChangeType::CREATED));
        assert!(kinds.contains(&FileChangeType::DELETED));
    }

    /// A theme file is read from disk, open in the editor or not.
    #[test]
    fn test_classify_events_flags_theme_yaml_even_when_open() {
        let open: HashSet<PathBuf> = HashSet::from([PathBuf::from("/ws/t-theme.yml")]);
        let changes = vec![make_event(
            "file:///ws/t-theme.yml",
            FileChangeType::CHANGED,
        )];
        let classified = classify_events(changes, &open).expect("theme event remains");
        assert!(classified.theme_changed);
        assert!(!classified.config_changed);
        assert!(classified.adoc_events.is_empty());
    }

    #[test]
    fn test_classify_events_returns_none_when_empty() {
        let open: HashSet<PathBuf> = HashSet::new();
        assert!(classify_events(vec![], &open).is_none());
    }

    #[test]
    fn test_classify_events_returns_none_when_only_unrelated() {
        let open: HashSet<PathBuf> = HashSet::new();
        let changes = vec![
            make_event("file:///ws/README.md", FileChangeType::CHANGED),
            make_event("file:///ws/Cargo.toml", FileChangeType::CHANGED),
        ];
        assert!(classify_events(changes, &open).is_none());
    }

    #[test]
    fn test_classify_events_filters_open_buffers() {
        let mut open: HashSet<PathBuf> = HashSet::new();
        open.insert(PathBuf::from("/ws/open.adoc"));
        let changes = vec![
            make_event("file:///ws/open.adoc", FileChangeType::CHANGED),
            make_event("file:///ws/closed.adoc", FileChangeType::CHANGED),
        ];
        let classified = classify_events(changes, &open).expect("closed.adoc remains");
        assert_eq!(classified.adoc_events.len(), 1);
        assert!(classified.adoc_events[0].0.ends_with("closed.adoc"));
    }

    #[test]
    fn test_classify_events_returns_none_when_only_open_buffer() {
        let mut open: HashSet<PathBuf> = HashSet::new();
        open.insert(PathBuf::from("/ws/open.adoc"));
        let changes = vec![make_event("file:///ws/open.adoc", FileChangeType::CHANGED)];
        assert!(classify_events(changes, &open).is_none());
    }

    /// Config chains are read from disk, so saving a config open in the
    /// editor (`.asciidoctorconfig.adoc` is an AsciiDoc buffer) must count.
    #[test]
    fn test_classify_events_keeps_config_open_in_editor() {
        let mut open: HashSet<PathBuf> = HashSet::new();
        open.insert(PathBuf::from("/ws/.asciidoctorconfig.adoc"));
        let changes = vec![make_event(
            "file:///ws/.asciidoctorconfig.adoc",
            FileChangeType::CHANGED,
        )];
        let classified = classify_events(changes, &open).expect("config event remains");
        assert!(classified.config_changed);
    }

    // --- asciidoc.explainPosition command arguments ---

    #[test]
    fn test_explain_position_args_parses_text_document_position() {
        let args = vec![serde_json::json!({
            "textDocument": {"uri": "file:///ws/a.adoc"},
            "position": {"line": 3, "character": 7},
        })];
        let params = parse_explain_position_command("asciidoc.explainPosition", args)
            .expect("valid arguments");
        assert_eq!(
            params.text_document.uri,
            Uri::from_str("file:///ws/a.adoc").unwrap()
        );
        assert_eq!(params.position, Position::new(3, 7));
    }

    #[test]
    fn test_explain_position_args_rejects_unknown_command() {
        let args = vec![serde_json::json!({
            "textDocument": {"uri": "file:///ws/a.adoc"},
            "position": {"line": 0, "character": 0},
        })];
        let err = parse_explain_position_command("asciidoc.somethingElse", args).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
    }

    #[test]
    fn test_explain_position_args_rejects_missing_argument() {
        let err = parse_explain_position_command("asciidoc.explainPosition", vec![]).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
    }

    #[test]
    fn test_explain_position_args_rejects_malformed_argument() {
        let args = vec![serde_json::json!({"textDocument": "not-an-object"})];
        let err = parse_explain_position_command("asciidoc.explainPosition", args).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
    }

    #[test]
    fn test_explain_position_args_rejects_extra_arguments() {
        let arg = serde_json::json!({
            "textDocument": {"uri": "file:///ws/a.adoc"},
            "position": {"line": 0, "character": 0},
        });
        let err =
            parse_explain_position_command("asciidoc.explainPosition", vec![arg.clone(), arg])
                .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
    }

    // --- debug log target format ---

    #[test]
    fn test_lsp_target_uses_lsp_range_convention() {
        let uri = Uri::from_str("file:///ws/a.adoc").unwrap();
        let range = Range {
            start: Position::new(2, 4),
            end: Position::new(2, 9),
        };
        assert_eq!(lsp_target(&uri, &range), "file:///ws/a.adoc:2:4-2:9");
    }

    // --- documents opened through a symlink ---

    /// A project at `<tmp>/real` with a directory symlink `<tmp>/link` to it.
    #[cfg(unix)]
    struct SymlinkedProject {
        _tmp: tempfile::TempDir,
        real: PathBuf,
        link: PathBuf,
    }

    #[cfg(unix)]
    impl SymlinkedProject {
        const DOC: &'static str = "= Doc\n:imagesdir: ../assets\n:my_attr: value\n\n\
                                   Text {my_attr}.\n\nimage::logo.svg[]\n\nimage::\n";
        const CONFIG_DOC: &'static str = "= Config\n\nimage::logo.svg[]\n";

        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let base = tmp.path().canonicalize().unwrap();
            let real = base.join("real");
            for dir in ["docs", "assets", "config"] {
                fs::create_dir_all(real.join(dir)).unwrap();
            }
            fs::write(real.join("docs/a.adoc"), Self::DOC).unwrap();
            fs::write(real.join("assets/logo.svg"), "<svg/>").unwrap();
            fs::write(real.join("config/ch2.adoc"), Self::CONFIG_DOC).unwrap();
            fs::write(
                real.join("config/.asciidoctorconfig"),
                format!(":imagesdir: {}\n", real.join("assets").display()),
            )
            .unwrap();
            let link = base.join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            Self {
                _tmp: tmp,
                real,
                link,
            }
        }
    }

    fn file_uri(path: &Path) -> Uri {
        Uri::from_file_path(path).unwrap()
    }

    /// The `ClientSocket` stays alive next to the service: dropping it would
    /// close the channel the server's client notifications go to.
    async fn initialized_server(
        root: &Path,
    ) -> (LspService<AsciiDocLsp>, tower_lsp_server::ClientSocket) {
        let (service, socket) = LspService::new(AsciiDocLsp::new);
        service
            .inner()
            .initialize(InitializeParams {
                workspace_folders: Some(vec![tower_lsp_server::ls_types::WorkspaceFolder {
                    uri: file_uri(root),
                    name: "test".into(),
                }]),
                ..Default::default()
            })
            .await
            .unwrap();
        service.inner().initialized(InitializedParams {}).await;
        scanned(service.inner()).await;
        (service, socket)
    }

    /// Wait until the workspace scan started by `initialized` is installed.
    async fn scanned(server: &AsciiDocLsp) {
        let install = server.scan.lock().take();
        if let Some(install) = install {
            install.await.unwrap();
        }
    }

    #[tokio::test]
    async fn test_did_open_and_did_change_strip_a_leading_bom() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let path = root.join("bom.adoc");
        fs::write(&path, "").unwrap();
        let uri = file_uri(&path);
        let (service, _socket) = initialized_server(&root).await;
        let server = service.inner();

        open_document(server, &uri, "\u{FEFF}:a: 1\n").await;
        assert_eq!(*server.get_document_content(&uri).unwrap(), ":a: 1\n");

        change_document(server, &uri, 2, "\u{FEFF}:b: 2\n").await;
        assert_eq!(*server.get_document_content(&uri).unwrap(), ":b: 2\n");
    }

    fn recalculations(server: &AsciiDocLsp) -> usize {
        server.recalculations.load(Ordering::SeqCst)
    }

    /// Forget the recalculations and publications so far.
    fn reset_observations(server: &AsciiDocLsp) {
        server.recalculations.store(0, Ordering::SeqCst);
        server.publications.lock().clear();
    }

    /// The diagnostics published for `uri`, one entry per publication.
    fn publications_for(server: &AsciiDocLsp, uri: &Uri) -> Vec<Vec<Diagnostic>> {
        server
            .publications
            .lock()
            .iter()
            .filter(|(published, _)| published == uri)
            .map(|(_, diagnostics)| diagnostics.clone())
            .collect()
    }

    /// Let the quiet period after the last edit pass. Under paused test time
    /// the runtime jumps to each timer, so this is instant and the debounced
    /// work, due earlier, has run when it returns.
    async fn quiet_period_passes() {
        tokio::time::sleep(DIAGNOSTICS_DEBOUNCE * 2).await;
    }

    /// A workspace of one clean file `a.adoc`, open in the editor.
    async fn server_with_open_file() -> (
        tempfile::TempDir,
        LspService<AsciiDocLsp>,
        tower_lsp_server::ClientSocket,
        Uri,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let path = root.join("a.adoc");
        fs::write(&path, "= A\n\nText.\n").unwrap();
        let uri = file_uri(&path);
        let (service, socket) = initialized_server(&root).await;
        open_document(service.inner(), &uri, "= A\n\nText.\n").await;
        (tmp, service, socket, uri)
    }

    #[tokio::test(start_paused = true)]
    async fn test_rapid_edits_recompute_the_diagnostics_once() {
        let (_tmp, service, _socket, uri) = server_with_open_file().await;
        let server = service.inner();
        reset_observations(server);

        for version in 2..201 {
            change_document(server, &uri, version, &format!("= A\n\nEdit {version}.\n")).await;
        }
        change_document(server, &uri, 201, "= A\n\nSee <<missing>>.\n").await;
        quiet_period_passes().await;

        assert_eq!(recalculations(server), 1);
        let published = publications_for(server, &uri);
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].len(), 1);
        assert!(published[0][0].message.contains("missing"));
    }

    #[tokio::test(start_paused = true)]
    async fn test_single_edit_gets_its_diagnostics_after_the_quiet_period() {
        let (_tmp, service, _socket, uri) = server_with_open_file().await;
        let server = service.inner();
        reset_observations(server);

        change_document(server, &uri, 2, "= A\n\nSee <<missing>>.\n").await;
        tokio::time::advance(DIAGNOSTICS_DEBOUNCE - std::time::Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(recalculations(server), 0);

        quiet_period_passes().await;
        assert_eq!(recalculations(server), 1);
        let published = publications_for(server, &uri);
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].len(), 1);
    }

    /// `a.adoc` defines the anchor `sec-a` and includes `b.adoc`, which
    /// refers to it; `c.adoc` has a broken xref. All on disk, `a.adoc` open.
    async fn server_with_dependent_files() -> (
        tempfile::TempDir,
        LspService<AsciiDocLsp>,
        tower_lsp_server::ClientSocket,
        [PathBuf; 3],
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let [a, b, c] = ["a.adoc", "b.adoc", "c.adoc"].map(|name| root.join(name));
        fs::write(&a, A_WITH_ANCHOR).unwrap();
        fs::write(&b, "See <<sec-a>>.\n").unwrap();
        fs::write(&c, "= C\n\nSee <<missing>>.\n").unwrap();
        let (service, socket) = initialized_server(&root).await;
        open_document(service.inner(), &file_uri(&a), A_WITH_ANCHOR).await;
        (tmp, service, socket, [a, b, c])
    }

    const A_WITH_ANCHOR: &str = "= A\n\n[[sec-a]]\n== S\n\ninclude::b.adoc[]\n";
    const A_WITHOUT_ANCHOR: &str = "= A\n\n== S\n\ninclude::b.adoc[]\n";

    #[tokio::test(start_paused = true)]
    async fn test_edit_does_not_republish_files_whose_diagnostics_stay() {
        let (_tmp, service, _socket, [a, b, c]) = server_with_dependent_files().await;
        let server = service.inner();
        reset_observations(server);

        let text = format!("{A_WITH_ANCHOR}\nMore.\n");
        change_document(server, &file_uri(&a), 2, &text).await;
        quiet_period_passes().await;

        assert_eq!(recalculations(server), 1);
        assert!(publications_for(server, &file_uri(&b)).is_empty());
        assert!(publications_for(server, &file_uri(&c)).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn test_edit_publishes_a_file_whose_diagnostics_change_and_clears_it_again() {
        let (_tmp, service, _socket, [a, b, _c]) = server_with_dependent_files().await;
        let server = service.inner();
        let b_uri = file_uri(&b);
        reset_observations(server);

        change_document(server, &file_uri(&a), 2, A_WITHOUT_ANCHOR).await;
        quiet_period_passes().await;
        let published = publications_for(server, &b_uri);
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].len(), 1);
        assert!(published[0][0].message.contains("sec-a"));

        reset_observations(server);
        change_document(server, &file_uri(&a), 3, A_WITH_ANCHOR).await;
        quiet_period_passes().await;
        assert_eq!(publications_for(server, &b_uri), vec![Vec::new()]);
    }

    /// Positions are converted against the text, so the edited file is
    /// published again even when its diagnostics are the same.
    #[tokio::test(start_paused = true)]
    async fn test_edit_republishes_the_edited_file_with_the_same_diagnostics() {
        let (_tmp, service, _socket, [_a, _b, c]) = server_with_dependent_files().await;
        let server = service.inner();
        let uri = file_uri(&c);
        let text = "= C\n\nSee <<missing>>.\n";
        open_document(server, &uri, text).await;
        reset_observations(server);

        change_document(server, &uri, 2, &format!("{text}\nMore.\n")).await;
        quiet_period_passes().await;

        assert_eq!(publications_for(server, &uri).len(), 1);
    }

    /// A file closed in the editor falls back to its disk text, which the
    /// positions of its diagnostics are converted against again.
    #[tokio::test(start_paused = true)]
    async fn test_close_republishes_the_file_with_the_same_diagnostics() {
        let (_tmp, service, _socket, [_a, _b, c]) = server_with_dependent_files().await;
        let server = service.inner();
        let uri = file_uri(&c);
        open_document(server, &uri, "= C\n\nSee <<missing>>. Typed.\n").await;
        reset_observations(server);

        server
            .did_close(DidCloseTextDocumentParams {
                text_document: document_id(&uri),
            })
            .await;

        assert_eq!(publications_for(server, &uri).len(), 1);
    }

    /// A file changed on disk is published again even when its diagnostics
    /// are the same; the others are not.
    #[tokio::test(start_paused = true)]
    async fn test_disk_change_republishes_only_the_changed_file() {
        let (_tmp, service, _socket, [_a, b, c]) = server_with_dependent_files().await;
        let server = service.inner();
        reset_observations(server);

        fs::write(&c, "= C\n\nSee <<missing>>. Changed.\n").unwrap();
        notify_watched(server, &c, FileChangeType::CHANGED).await;

        assert_eq!(publications_for(server, &file_uri(&c)).len(), 1);
        assert!(publications_for(server, &file_uri(&b)).is_empty());
    }

    /// Give `path` the buffer `text` and recompute, as an edit and its
    /// debounced work do, without publishing.
    fn recompute_with_buffer(server: &AsciiDocLsp, path: &Path, text: &str) -> Recomputed {
        let uri = file_uri(path);
        server.documents.lock().insert(
            uri.clone(),
            DocumentState {
                content: Arc::new(text.into()),
                version: 100,
            },
        );
        let overrides = build_content_overrides(&server.documents, &uri);
        let mut guard = server.workspace.lock();
        let ws = guard.as_mut().unwrap();
        ws.update_file(path, &overrides);
        server.content_changed.lock().insert(path.to_path_buf());
        server.recompute_diagnostics(ws)
    }

    /// Two publications that overtake each other: the older one, arriving
    /// last, does not put its diagnostics back on the client.
    #[tokio::test(start_paused = true)]
    async fn test_older_recomputation_published_last_does_not_win() {
        let (_tmp, service, _socket, [a, b, _c]) = server_with_dependent_files().await;
        let server = service.inner();
        let older = recompute_with_buffer(server, &a, A_WITHOUT_ANCHOR);
        let newer = recompute_with_buffer(server, &a, A_WITH_ANCHOR);
        reset_observations(server);

        server.publish_workspace_diagnostics(&newer).await;
        server.publish_workspace_diagnostics(&older).await;

        let published = publications_for(server, &file_uri(&b));
        assert!(published.last().is_none_or(Vec::is_empty), "{published:?}");
    }

    /// A file whose text changed before the older of two overtaking
    /// recomputations is still converted against its new text.
    #[tokio::test(start_paused = true)]
    async fn test_older_recomputation_published_last_keeps_its_changed_text() {
        let (_tmp, service, _socket, [a, _b, c]) = server_with_dependent_files().await;
        let server = service.inner();
        server.content_changed.lock().insert(c.clone());
        let older = recompute_with_buffer(server, &a, A_WITH_ANCHOR);
        let newer = recompute_with_buffer(server, &a, &format!("{A_WITH_ANCHOR}\nMore.\n"));
        reset_observations(server);

        server.publish_workspace_diagnostics(&newer).await;
        server.publish_workspace_diagnostics(&older).await;

        let published = publications_for(server, &file_uri(&c));
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].len(), 1);
    }

    /// A save within the quiet period recomputes at once and covers the
    /// edit: the debounced recomputation is dropped.
    #[tokio::test(start_paused = true)]
    async fn test_save_after_an_edit_cancels_the_debounced_recomputation() {
        let (_tmp, service, _socket, uri) = server_with_open_file().await;
        let server = service.inner();
        reset_observations(server);

        change_document(server, &uri, 2, "= A\n\nSee <<missing>>.\n").await;
        server
            .did_save(DidSaveTextDocumentParams {
                text_document: document_id(&uri),
                text: None,
            })
            .await;
        assert_eq!(recalculations(server), 1);
        quiet_period_passes().await;

        assert_eq!(recalculations(server), 1);
        assert_eq!(publications_for(server, &uri).len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn test_did_open_recomputes_the_diagnostics_at_once() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let path = root.join("a.adoc");
        fs::write(&path, "= A\n").unwrap();
        let (service, _socket) = initialized_server(&root).await;
        let server = service.inner();
        reset_observations(server);

        let uri = file_uri(&path);
        open_document(server, &uri, "= A\n\nSee <<missing>>.\n").await;

        assert_eq!(recalculations(server), 1);
        assert_eq!(publications_for(server, &uri).len(), 1);
    }

    #[tokio::test]
    async fn test_buffer_opened_during_the_scan_wins_over_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let opened = root.join("opened.adoc");
        let closed = root.join("closed.adoc");
        fs::write(&opened, "= Opened\n\nSee <<missing>>.\n").unwrap();
        fs::write(&closed, "= Closed\n\nSee <<missing>>.\n").unwrap();
        let (service, _socket) = LspService::new(AsciiDocLsp::new);
        let server = service.inner();
        server
            .initialize(InitializeParams {
                workspace_folders: Some(vec![tower_lsp_server::ls_types::WorkspaceFolder {
                    uri: file_uri(&root),
                    name: "test".into(),
                }]),
                ..Default::default()
            })
            .await
            .unwrap();
        server.initialized(InitializedParams {}).await;

        // The test runtime runs the install only once the test awaits it.
        let uri = file_uri(&opened);
        open_document(server, &uri, "= Opened\n\nNo xref.\n").await;
        assert!(server.workspace.lock().is_none());
        assert_eq!(definition(server, &uri, Position::new(0, 2)).await, None);

        scanned(server).await;
        let diagnostics = server.diagnostics.lock().clone();
        assert_eq!(diagnostics.get(&opened).map(Vec::len).unwrap_or(0), 0);
        assert_eq!(diagnostics.get(&closed).map(Vec::len), Some(1));
    }

    #[tokio::test]
    async fn test_request_during_the_scan_waits_for_the_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let index = root.join("index.adoc");
        let target = root.join("target.adoc");
        let text = "= Index\n\ninclude::target.adoc[]\n\nSee <<goal>>.\n";
        fs::write(&index, text).unwrap();
        fs::write(&target, "[[goal]]\n== Goal\n").unwrap();
        let (service, _socket) = LspService::new(AsciiDocLsp::new);
        let server = service.inner();
        server
            .initialize(InitializeParams {
                workspace_folders: Some(vec![tower_lsp_server::ls_types::WorkspaceFolder {
                    uri: file_uri(&root),
                    name: "test".into(),
                }]),
                ..Default::default()
            })
            .await
            .unwrap();
        server.initialized(InitializedParams {}).await;

        let uri = file_uri(&index);
        open_document(server, &uri, text).await;
        assert!(server.workspace.lock().is_none());
        let location = definition(server, &uri, Position::new(4, 7)).await;

        assert_eq!(location.map(|l| l.uri), Some(file_uri(&target)));
    }

    #[tokio::test]
    async fn test_buffer_changed_during_the_scan_wins_over_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let opened = root.join("opened.adoc");
        let disk = "= Opened\n\nSee <<missing>>.\n";
        fs::write(&opened, disk).unwrap();
        let (service, _socket) = LspService::new(AsciiDocLsp::new);
        let server = service.inner();
        server
            .initialize(InitializeParams {
                workspace_folders: Some(vec![tower_lsp_server::ls_types::WorkspaceFolder {
                    uri: file_uri(&root),
                    name: "test".into(),
                }]),
                ..Default::default()
            })
            .await
            .unwrap();
        server.initialized(InitializedParams {}).await;

        // The test runtime runs the install only once the test awaits it.
        let uri = file_uri(&opened);
        open_document(server, &uri, disk).await;
        change_document(server, &uri, 2, "= Opened\n\nNo xref.\n").await;
        assert!(server.workspace.lock().is_none());

        scanned(server).await;
        let diagnostics = server.diagnostics.lock().clone();
        assert_eq!(diagnostics.get(&opened).map(Vec::len).unwrap_or(0), 0);
    }

    /// A server whose scan of `root` has read the disk but is not installed:
    /// the test runtime runs the install only once the test awaits it.
    async fn server_with_scan_read(
        root: &Path,
    ) -> (LspService<AsciiDocLsp>, tower_lsp_server::ClientSocket) {
        let (service, socket) = LspService::new(AsciiDocLsp::new);
        let server = service.inner();
        server
            .initialize(InitializeParams {
                workspace_folders: Some(vec![tower_lsp_server::ls_types::WorkspaceFolder {
                    uri: file_uri(root),
                    name: "test".into(),
                }]),
                ..Default::default()
            })
            .await
            .unwrap();
        server.initialized(InitializedParams {}).await;
        let thread = server.scan_thread.lock().take().unwrap();
        thread.join().unwrap();
        assert!(server.workspace.lock().is_none());
        (service, socket)
    }

    #[tokio::test]
    async fn test_file_changed_on_disk_during_the_scan_counts_after_install() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let path = root.join("a.adoc");
        fs::write(&path, "= A\n\nSee <<missing>>.\n").unwrap();
        let (service, _socket) = server_with_scan_read(&root).await;
        let server = service.inner();

        fs::write(&path, "= A\n\nNo xref.\n").unwrap();
        notify_watched(server, &path, FileChangeType::CHANGED).await;
        assert!(server.workspace.lock().is_none());

        scanned(server).await;
        let diagnostics = server.diagnostics.lock().clone();
        assert_eq!(diagnostics.get(&path).map(Vec::len).unwrap_or(0), 0);
    }

    #[tokio::test]
    async fn test_file_created_on_disk_during_the_scan_counts_after_install() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (service, _socket) = server_with_scan_read(&root).await;
        let server = service.inner();

        let path = root.join("new.adoc");
        fs::write(&path, "= New\n\nSee <<missing>>.\n").unwrap();
        notify_watched(server, &path, FileChangeType::CREATED).await;
        assert!(server.workspace.lock().is_none());

        scanned(server).await;
        let diagnostics = server.diagnostics.lock().clone();
        assert_eq!(diagnostics.get(&path).map(Vec::len), Some(1));
    }

    #[tokio::test]
    async fn test_file_deleted_on_disk_during_the_scan_is_gone_after_install() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let path = root.join("a.adoc");
        fs::write(&path, "= A\n\nSee <<missing>>.\n").unwrap();
        let (service, _socket) = server_with_scan_read(&root).await;
        let server = service.inner();

        fs::remove_file(&path).unwrap();
        notify_watched(server, &path, FileChangeType::DELETED).await;
        assert!(server.workspace.lock().is_none());

        scanned(server).await;
        let indexed = server
            .workspace
            .lock()
            .as_ref()
            .unwrap()
            .documents()
            .contains_key(&path);
        assert!(!indexed);
        assert!(server.diagnostics.lock().get(&path).is_none());
    }

    #[tokio::test]
    async fn test_config_changed_during_the_scan_counts_after_install() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let path = root.join("a.adoc");
        let config = root.join(".asciidoctorconfig");
        fs::write(&path, "= A\n\nGreeting: {greeting}\n").unwrap();
        fs::write(&config, ":other: x\n").unwrap();
        let (service, _socket) = server_with_scan_read(&root).await;
        let server = service.inner();

        fs::write(&config, ":greeting: hello\n").unwrap();
        notify_watched(server, &config, FileChangeType::CHANGED).await;
        assert!(server.workspace.lock().is_none());

        scanned(server).await;
        let diagnostics = server.diagnostics.lock().clone();
        assert_eq!(diagnostics.get(&path).map(Vec::len).unwrap_or(0), 0);
    }

    #[tokio::test]
    async fn test_buffer_saved_and_closed_during_the_scan_counts_from_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let path = root.join("a.adoc");
        let old = "= A\n\nSee <<missing>>.\n";
        fs::write(&path, old).unwrap();
        let (service, _socket) = server_with_scan_read(&root).await;
        let server = service.inner();

        let uri = file_uri(&path);
        let saved = "= A\n\nNo xref.\n";
        open_document(server, &uri, old).await;
        change_document(server, &uri, 2, saved).await;
        fs::write(&path, saved).unwrap();
        server
            .did_save(DidSaveTextDocumentParams {
                text_document: document_id(&uri),
                text: None,
            })
            .await;
        server
            .did_close(DidCloseTextDocumentParams {
                text_document: document_id(&uri),
            })
            .await;
        assert!(server.workspace.lock().is_none());

        scanned(server).await;
        let diagnostics = server.diagnostics.lock().clone();
        assert_eq!(diagnostics.get(&path).map(Vec::len).unwrap_or(0), 0);
    }

    /// A disk change reported during the scan does not replace a buffer that
    /// is open when the workspace is installed.
    #[tokio::test]
    async fn test_buffer_opened_after_a_disk_change_during_the_scan_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let path = root.join("a.adoc");
        fs::write(&path, "= A\n\nNo xref.\n").unwrap();
        let (service, _socket) = server_with_scan_read(&root).await;
        let server = service.inner();

        fs::write(&path, "= A\n\nSee <<missing>>.\n").unwrap();
        notify_watched(server, &path, FileChangeType::CHANGED).await;
        open_document(server, &file_uri(&path), "= A\n\nStill no xref.\n").await;

        scanned(server).await;
        let diagnostics = server.diagnostics.lock().clone();
        assert_eq!(diagnostics.get(&path).map(Vec::len).unwrap_or(0), 0);
    }

    /// Without a workspace root no scan runs: file changes are not kept for
    /// an install that never comes.
    #[tokio::test]
    async fn test_file_changes_without_a_root_are_not_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().canonicalize().unwrap().join("a.adoc");
        fs::write(&path, "= A\n").unwrap();
        let (service, _socket) = LspService::new(AsciiDocLsp::new);
        let server = service.inner();
        server
            .initialize(InitializeParams::default())
            .await
            .unwrap();
        server.initialized(InitializedParams {}).await;

        notify_watched(server, &path, FileChangeType::CHANGED).await;
        server
            .did_save(DidSaveTextDocumentParams {
                text_document: document_id(&file_uri(&path)),
                text: None,
            })
            .await;

        assert!(server.pending_changes.lock().is_none());
    }

    #[tokio::test]
    async fn test_initialize_reports_server_info() {
        let (service, _socket) = LspService::new(AsciiDocLsp::new);

        let result = service
            .inner()
            .initialize(InitializeParams::default())
            .await
            .unwrap();

        let info = result.server_info.unwrap();
        assert_eq!(info.name, "asciidoc-lsp");
        assert_eq!(
            info.version.as_deref(),
            Some(version::build_version().as_str())
        );
    }

    async fn open_document(server: &AsciiDocLsp, uri: &Uri, text: &str) {
        server
            .did_open(DidOpenTextDocumentParams {
                text_document: tower_lsp_server::ls_types::TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "asciidoc".into(),
                    version: 1,
                    text: text.into(),
                },
            })
            .await;
    }

    async fn change_document(server: &AsciiDocLsp, uri: &Uri, version: i32, text: &str) {
        server
            .did_change(DidChangeTextDocumentParams {
                text_document: tower_lsp_server::ls_types::VersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version,
                },
                content_changes: vec![tower_lsp_server::ls_types::TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: text.into(),
                }],
            })
            .await;
    }

    fn document_id(uri: &Uri) -> tower_lsp_server::ls_types::TextDocumentIdentifier {
        tower_lsp_server::ls_types::TextDocumentIdentifier { uri: uri.clone() }
    }

    fn position_of(text: &str, needle: &str, offset: u32) -> Position {
        let idx = text.find(needle).unwrap();
        let before = &text[..idx];
        let line = u32::try_from(before.matches('\n').count()).unwrap();
        let column = idx - before.rfind('\n').map_or(0, |nl| nl + 1);
        Position::new(line, u32::try_from(column).unwrap() + offset)
    }

    fn doc_position(uri: &Uri, position: Position) -> TextDocumentPositionParams {
        TextDocumentPositionParams {
            text_document: tower_lsp_server::ls_types::TextDocumentIdentifier { uri: uri.clone() },
            position,
        }
    }

    async fn hover_text(server: &AsciiDocLsp, uri: &Uri, position: Position) -> Option<String> {
        let hover = server
            .hover(HoverParams {
                text_document_position_params: doc_position(uri, position),
                work_done_progress_params: Default::default(),
            })
            .await
            .unwrap()?;
        match hover.contents {
            HoverContents::Scalar(MarkedString::String(text)) => Some(text),
            other => panic!("unexpected hover contents: {other:?}"),
        }
    }

    async fn definition(server: &AsciiDocLsp, uri: &Uri, position: Position) -> Option<Location> {
        let response = server
            .goto_definition(GotoDefinitionParams {
                text_document_position_params: doc_position(uri, position),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await
            .unwrap()?;
        match response {
            GotoDefinitionResponse::Scalar(location) => Some(location),
            other => panic!("unexpected definition response: {other:?}"),
        }
    }

    async fn completion_labels(server: &AsciiDocLsp, uri: &Uri, position: Position) -> Vec<String> {
        let response = server
            .completion(CompletionParams {
                text_document_position: doc_position(uri, position),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: None,
            })
            .await
            .unwrap();
        match response {
            Some(CompletionResponse::Array(items)) => {
                let mut labels: Vec<String> = items.into_iter().map(|i| i.label).collect();
                labels.sort();
                labels
            }
            Some(other) => panic!("unexpected completion response: {other:?}"),
            None => vec![],
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_symlinked_document_resolves_attribute() {
        let project = SymlinkedProject::new();
        let doc = SymlinkedProject::DOC;
        for root in [&project.real, &project.link] {
            let (service, _socket) = initialized_server(root).await;
            let server = service.inner();
            let uri = file_uri(&project.link.join("docs/a.adoc"));
            open_document(server, &uri, doc).await;
            let position = position_of(doc, "{my_attr}", 3);

            let hover = hover_text(server, &uri, position).await;
            let target = definition(server, &uri, position).await;

            assert_eq!(
                hover.as_deref(),
                Some(":my_attr: value\n\nDefined in a.adoc:3")
            );
            assert_eq!(target.unwrap().range.start.line, 2);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_symlinked_document_uses_document_imagesdir() {
        let project = SymlinkedProject::new();
        let doc = SymlinkedProject::DOC;
        for root in [&project.real, &project.link] {
            let (service, _socket) = initialized_server(root).await;
            let server = service.inner();
            let uri = file_uri(&project.link.join("docs/a.adoc"));
            open_document(server, &uri, doc).await;
            let position = position_of(doc, "image::logo.svg", 9);

            let hover = hover_text(server, &uri, position).await.unwrap();
            let target = definition(server, &uri, position).await.unwrap();

            assert_eq!(hover, "image::../assets/logo.svg[]\n\n✓ File exists");
            assert_eq!(target.uri, file_uri(&project.real.join("assets/logo.svg")));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_symlinked_document_uses_config_imagesdir() {
        let project = SymlinkedProject::new();
        let doc = SymlinkedProject::CONFIG_DOC;
        let (service, _socket) = initialized_server(&project.link).await;
        let server = service.inner();
        let uri = file_uri(&project.link.join("config/ch2.adoc"));
        open_document(server, &uri, doc).await;
        let position = position_of(doc, "image::logo.svg", 9);

        let target = definition(server, &uri, position).await.unwrap();

        assert_eq!(target.uri, file_uri(&project.real.join("assets/logo.svg")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_symlinked_document_completes_image_paths() {
        let project = SymlinkedProject::new();
        let doc = SymlinkedProject::DOC;
        for root in [&project.real, &project.link] {
            let (service, _socket) = initialized_server(root).await;
            let server = service.inner();
            let uri = file_uri(&project.link.join("docs/a.adoc"));
            open_document(server, &uri, doc).await;
            let position = position_of(doc, "image::\n", 7);

            let labels = completion_labels(server, &uri, position).await;

            assert_eq!(labels, vec!["logo.svg".to_string()]);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_symlinked_buffer_content_reaches_the_workspace() {
        let project = SymlinkedProject::new();
        let (service, _socket) = initialized_server(&project.real).await;
        let server = service.inner();
        let uri = file_uri(&project.link.join("docs/a.adoc"));
        let unsaved = "= Doc\n:unsaved: buffer\n\nText {unsaved}.\n";
        open_document(server, &uri, unsaved).await;
        let position = position_of(unsaved, "{unsaved}", 3);

        let hover = hover_text(server, &uri, position).await;

        assert_eq!(
            hover.as_deref(),
            Some(":unsaved: buffer\n\nDefined in a.adoc:2")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_symlinked_buffer_is_registered_once() {
        let project = SymlinkedProject::new();
        let (service, _socket) = initialized_server(&project.link).await;
        let server = service.inner();
        let uri = file_uri(&project.link.join("docs/a.adoc"));
        open_document(server, &uri, SymlinkedProject::DOC).await;

        let ws = server.workspace.lock();
        let keys: Vec<&PathBuf> = ws
            .as_ref()
            .unwrap()
            .documents()
            .keys()
            .filter(|path| path.ends_with("docs/a.adoc"))
            .collect();

        assert_eq!(keys, vec![&project.real.join("docs/a.adoc")]);
    }

    #[cfg(unix)]
    #[test]
    fn test_content_overrides_key_a_symlinked_buffer_canonically() {
        let project = SymlinkedProject::new();
        let docs: Mutex<HashMap<Uri, DocumentState>> = Mutex::new(HashMap::new());
        let uri = file_uri(&project.link.join("docs/a.adoc"));
        docs.lock().insert(
            uri.clone(),
            DocumentState {
                content: Arc::new("buffer".into()),
                version: 1,
            },
        );

        let overrides = build_content_overrides(&docs, &uri);

        assert_eq!(overrides.len(), 1);
        assert_eq!(
            overrides[&project.real.join("docs/a.adoc")].as_str(),
            "buffer"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_content_overrides_prefer_the_active_buffer_of_one_file() {
        let project = SymlinkedProject::new();
        let docs: Mutex<HashMap<Uri, DocumentState>> = Mutex::new(HashMap::new());
        let via_link = file_uri(&project.link.join("docs/a.adoc"));
        let via_real = file_uri(&project.real.join("docs/a.adoc"));
        for (uri, content) in [(&via_link, "link buffer"), (&via_real, "real buffer")] {
            docs.lock().insert(
                uri.clone(),
                DocumentState {
                    content: Arc::new(content.into()),
                    version: 1,
                },
            );
        }
        let canonical = project.real.join("docs/a.adoc");

        let active_link = build_content_overrides(&docs, &via_link);
        let active_real = build_content_overrides(&docs, &via_real);

        assert_eq!(active_link.len(), 1);
        assert_eq!(active_link[&canonical].as_str(), "link buffer");
        assert_eq!(active_real[&canonical].as_str(), "real buffer");
    }

    async fn references_of(server: &AsciiDocLsp, uri: &Uri, position: Position) -> Vec<Location> {
        server
            .references(ReferenceParams {
                text_document_position: doc_position(uri, position),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: tower_lsp_server::ls_types::ReferenceContext {
                    include_declaration: true,
                },
            })
            .await
            .unwrap()
            .unwrap()
    }

    async fn rename_edits(
        server: &AsciiDocLsp,
        uri: &Uri,
        position: Position,
    ) -> HashMap<Uri, Vec<TextEdit>> {
        server
            .rename(RenameParams {
                text_document_position: doc_position(uri, position),
                new_name: "renamed".into(),
                work_done_progress_params: Default::default(),
            })
            .await
            .unwrap()
            .unwrap()
            .changes
            .unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_same_file_definition_stays_on_the_client_uri() {
        let project = SymlinkedProject::new();
        let doc = SymlinkedProject::DOC;
        for root in [&project.real, &project.link] {
            let (service, _socket) = initialized_server(root).await;
            let server = service.inner();
            let uri = file_uri(&project.link.join("docs/a.adoc"));
            open_document(server, &uri, doc).await;

            let target = definition(server, &uri, position_of(doc, "{my_attr}", 3)).await;

            assert_eq!(target.unwrap().uri, uri);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_same_file_references_stay_on_the_client_uri() {
        let project = SymlinkedProject::new();
        let doc = SymlinkedProject::DOC;
        let (service, _socket) = initialized_server(&project.link).await;
        let server = service.inner();
        let uri = file_uri(&project.link.join("docs/a.adoc"));
        open_document(server, &uri, doc).await;

        let locations = references_of(server, &uri, position_of(doc, "{my_attr}", 3)).await;

        let lines: Vec<u32> = locations.iter().map(|l| l.range.start.line).collect();
        assert_eq!(lines, vec![2, 4], "declaration and usage");
        assert!(locations.iter().all(|l| l.uri == uri));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_same_file_rename_stays_on_the_client_uri() {
        let project = SymlinkedProject::new();
        let doc = SymlinkedProject::DOC;
        let (service, _socket) = initialized_server(&project.link).await;
        let server = service.inner();
        let uri = file_uri(&project.link.join("docs/a.adoc"));
        open_document(server, &uri, doc).await;

        let changes = rename_edits(server, &uri, position_of(doc, "{my_attr}", 3)).await;

        assert_eq!(changes.keys().collect::<Vec<_>>(), vec![&uri]);
        assert_eq!(changes[&uri].len(), 2, "declaration and usage");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_rename_reads_the_symlinked_buffer_not_the_disk() {
        let project = SymlinkedProject::new();
        let (service, _socket) = initialized_server(&project.real).await;
        let server = service.inner();
        let uri = file_uri(&project.link.join("docs/a.adoc"));
        let unsaved = "= Doc\n\n\n:shifted: value\n\nText {shifted}.\n";
        open_document(server, &uri, unsaved).await;

        let changes = rename_edits(server, &uri, position_of(unsaved, "{shifted}", 3)).await;

        let lines: Vec<u32> = changes[&uri].iter().map(|e| e.range.start.line).collect();
        assert!(lines.contains(&3), "definition from the buffer: {lines:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_cross_file_result_uses_the_uri_of_an_open_buffer() {
        let project = SymlinkedProject::new();
        fs::write(project.real.join("docs/b.adoc"), "[[target]]\n== B\n").unwrap();
        let doc = "= Doc\n\nSee xref:b.adoc#target[B].\n";
        let (service, _socket) = initialized_server(&project.link).await;
        let server = service.inner();
        let uri = file_uri(&project.link.join("docs/a.adoc"));
        open_document(server, &uri, doc).await;
        let position = position_of(doc, "b.adoc#target", 3);

        let closed = definition(server, &uri, position).await.unwrap();
        let b_link = file_uri(&project.link.join("docs/b.adoc"));
        open_document(server, &b_link, "[[target]]\n== B\n").await;
        let open = definition(server, &uri, position).await.unwrap();

        assert_eq!(closed.uri, file_uri(&project.real.join("docs/b.adoc")));
        assert_eq!(open.uri, b_link);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_same_file_result_follows_the_requesting_buffer() {
        let project = SymlinkedProject::new();
        let doc = SymlinkedProject::DOC;
        let (service, _socket) = initialized_server(&project.real).await;
        let server = service.inner();
        let via_link = file_uri(&project.link.join("docs/a.adoc"));
        let via_real = file_uri(&project.real.join("docs/a.adoc"));
        open_document(server, &via_link, doc).await;
        open_document(server, &via_real, doc).await;
        let position = position_of(doc, "{my_attr}", 3);

        let from_link = definition(server, &via_link, position).await.unwrap();
        let from_real = definition(server, &via_real, position).await.unwrap();

        assert_eq!(from_link.uri, via_link);
        assert_eq!(from_real.uri, via_real);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_explain_position_reports_what_the_handlers_look_up() {
        let project = SymlinkedProject::new();
        let doc = SymlinkedProject::DOC;
        let (service, _socket) = initialized_server(&project.real).await;
        let server = service.inner();
        let raw = project.link.join("docs/a.adoc");
        let canonical = project.real.join("docs/a.adoc");
        let uri = file_uri(&raw);
        open_document(server, &uri, doc).await;
        let argument =
            serde_json::to_value(doc_position(&uri, position_of(doc, "{my_attr}", 3))).unwrap();

        let report = server
            .execute_command(ExecuteCommandParams {
                command: EXPLAIN_POSITION_COMMAND.into(),
                arguments: vec![argument],
                work_done_progress_params: Default::default(),
            })
            .await
            .unwrap()
            .unwrap();

        let request = &report["request"];
        assert_eq!(request["raw_path"], raw.display().to_string());
        assert_eq!(request["canonical_path"], canonical.display().to_string());
        assert_eq!(request["lookup_path"], canonical.display().to_string());
        assert_eq!(request["paths_differ"], true);
        assert_eq!(request["in_documents_raw"], false);
        assert_eq!(request["in_documents_canonical"], true);
        assert_eq!(report["resolution"]["scopes"][0]["value"], "value");
    }

    #[tokio::test]
    async fn test_xref_subdirectory_completion_over_the_protocol() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("chapters")).unwrap();
        fs::write(tmp.path().join("chapters/intro.adoc"), "= Intro\n").unwrap();
        let doc = "= Doc\n\nSee xref:chapters/\n";
        let (service, _socket) = initialized_server(tmp.path()).await;
        let server = service.inner();
        let uri = file_uri(&tmp.path().join("index.adoc"));
        open_document(server, &uri, doc).await;

        let response = server
            .completion(CompletionParams {
                text_document_position: doc_position(&uri, position_of(doc, "xref:chapters/", 14)),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: None,
            })
            .await
            .unwrap();

        let Some(CompletionResponse::Array(items)) = response else {
            panic!("expected completion items, got {response:?}");
        };
        let intro = items.iter().find(|i| i.label == "intro.adoc").unwrap();
        // The edit replaces the typed `chapters/` instead of leaving it to the
        // client's keyword rules, which stop at `/`.
        let Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(edit)) = &intro.text_edit
        else {
            panic!("expected a TextEdit, got {:?}", intro.text_edit);
        };
        assert_eq!(
            edit.range,
            Range::new(Position::new(2, 9), Position::new(2, 18))
        );
        assert_eq!(edit.new_text, "chapters/intro.adoc${1}[${2:label}]");
        assert_eq!(intro.filter_text.as_deref(), Some("chapters/intro.adoc"));
    }

    #[tokio::test]
    async fn test_slash_triggers_completion() {
        let tmp = tempfile::tempdir().unwrap();
        let (service, _socket) = LspService::new(AsciiDocLsp::new);
        let result = service
            .inner()
            .initialize(InitializeParams {
                workspace_folders: Some(vec![tower_lsp_server::ls_types::WorkspaceFolder {
                    uri: file_uri(tmp.path()),
                    name: "test".into(),
                }]),
                ..Default::default()
            })
            .await
            .unwrap();

        let triggers = result
            .capabilities
            .completion_provider
            .and_then(|c| c.trigger_characters)
            .unwrap_or_default();
        assert!(triggers.iter().any(|t| t == "/"), "triggers: {triggers:?}");
    }

    #[tokio::test]
    async fn test_bracket_triggers_completion_but_comma_does_not() {
        let tmp = tempfile::tempdir().unwrap();
        let (service, _socket) = LspService::new(AsciiDocLsp::new);
        let result = service
            .inner()
            .initialize(InitializeParams {
                workspace_folders: Some(vec![tower_lsp_server::ls_types::WorkspaceFolder {
                    uri: file_uri(tmp.path()),
                    name: "test".into(),
                }]),
                ..Default::default()
            })
            .await
            .unwrap();

        let triggers = result
            .capabilities
            .completion_provider
            .and_then(|c| c.trigger_characters)
            .unwrap_or_default();
        assert!(triggers.iter().any(|t| t == "["), "triggers: {triggers:?}");
        assert!(!triggers.iter().any(|t| t == ","), "triggers: {triggers:?}");
    }

    #[tokio::test]
    async fn test_block_attribute_line_completes_style_and_language() {
        let tmp = tempfile::tempdir().unwrap();
        let (service, _socket) = initialized_server(tmp.path()).await;
        let server = service.inner();
        let uri = file_uri(&tmp.path().join("doc.adoc"));
        let text = "= Doc\n\n[NO\n\n[source,ru\n";
        open_document(server, &uri, text).await;

        let styles = completion_labels(server, &uri, position_of(text, "[NO", 3)).await;
        let languages = completion_labels(server, &uri, position_of(text, "[source,ru", 10)).await;

        assert!(styles.iter().any(|l| l == "NOTE"), "labels: {styles:?}");
        assert!(
            languages.iter().any(|l| l == "rust"),
            "labels: {languages:?}"
        );
    }

    #[tokio::test]
    async fn test_initialize_offers_full_semantic_tokens_only() {
        let (service, _socket) = LspService::new(AsciiDocLsp::new);

        let result = service
            .inner()
            .initialize(InitializeParams::default())
            .await
            .unwrap();

        let Some(SemanticTokensServerCapabilities::SemanticTokensOptions(options)) =
            result.capabilities.semantic_tokens_provider
        else {
            panic!("no semantic tokens provider");
        };
        assert_eq!(
            options.full,
            Some(SemanticTokensFullOptions::Bool(true)),
            "full requests only: no delta"
        );
        assert_eq!(options.range, None, "no range requests");
        assert_eq!(options.legend, semantic_tokens::legend());
        // Markup has no standard type, so the legend carries custom ones next
        // to standard ones; the extension declares the custom ones.
        let types: Vec<&str> = options
            .legend
            .token_types
            .iter()
            .map(SemanticTokenType::as_str)
            .collect();
        assert!(types.contains(&"heading"), "types: {types:?}");
        assert!(types.contains(&"comment"), "types: {types:?}");
    }

    #[test]
    fn test_document_path_keeps_the_raw_path_of_an_unsaved_file() {
        let uri = Uri::from_str("file:///does/not/exist/new.adoc").unwrap();

        assert_eq!(
            document_path(&uri),
            Some(PathBuf::from("/does/not/exist/new.adoc"))
        );
    }

    // --- Committed fixtures: tests/fixtures/symlinks, tests/fixtures/ventilate ---
    //
    // In `symlinks/`, `linked` points to `real/project` and
    // `real/project/shared-assets` to `real/outside/shared`. Scenarios:
    // A canonical root and path, B symlinked root, C canonical root with a
    // symlinked document path, D/E imagesdir from `.asciidoctorconfig`
    // (canonical/symlinked), F an imagesdir that is itself a symlink.
    // The other links (`other/*.adoc`, `real/project/guide`,
    // `config/book-link`, `config/outside.adoc`, `docs/outside.adoc`, `home`)
    // are described at the tests that use them.

    fn fixture_dir(name: &str) -> PathBuf {
        workspace_root().join("tests/fixtures").join(name)
    }

    /// Opens `doc` with its text from disk, addressed by `doc` as given.
    async fn open_fixture(
        root: &Path,
        doc: &Path,
    ) -> (
        LspService<AsciiDocLsp>,
        tower_lsp_server::ClientSocket,
        Uri,
        String,
    ) {
        let text = fs::read_to_string(doc).unwrap();
        let (service, socket) = initialized_server(root).await;
        let uri = file_uri(doc);
        open_document(service.inner(), &uri, &text).await;
        (service, socket, uri, text)
    }

    async fn explain(server: &AsciiDocLsp, uri: &Uri, position: Position) -> serde_json::Value {
        let argument = serde_json::to_value(doc_position(uri, position)).unwrap();
        server
            .execute_command(ExecuteCommandParams {
                command: EXPLAIN_POSITION_COMMAND.into(),
                arguments: vec![argument],
                work_done_progress_params: Default::default(),
            })
            .await
            .unwrap()
            .unwrap()
    }

    /// Scenarios A, B and C: `docs/ch1.adoc` through canonical and symlinked paths.
    fn ch1_scenarios(fx: &Path) -> [(&'static str, PathBuf, PathBuf); 3] {
        let real = fx.join("real/project");
        let linked = fx.join("linked");
        [
            ("A", real.clone(), real.join("docs/ch1.adoc")),
            ("B", linked.clone(), linked.join("docs/ch1.adoc")),
            ("C", real, linked.join("docs/ch1.adoc")),
        ]
    }

    /// Hover, gd and completion in `ch1.adoc`, one line per probe. A gd target
    /// in the requesting document is written as `<self>`: B and C differ from
    /// A only there, by design.
    async fn observe_ch1(server: &AsciiDocLsp, uri: &Uri, text: &str, fx: &Path) -> Vec<String> {
        let mut seen = Vec::new();
        for (needle, offset) in [
            ("image::logo.svg", 9),
            ("image::{logo-name}", 7),
            ("image::sub/nested", 12),
            ("image::missing", 9),
            ("{my_variable}", 3),
        ] {
            let position = position_of(text, needle, offset);
            let hover = hover_text(server, uri, position).await;
            let target = definition(server, uri, position).await.map(|location| {
                let file = if location.uri == *uri {
                    "<self>".to_string()
                } else {
                    let path = location.uri.to_file_path().unwrap();
                    let relative = path.strip_prefix(fx).unwrap();
                    format!("<fx>/{}", relative.display())
                };
                format!("{file}:{}", location.range.start.line)
            });
            seen.push(format!(
                "{needle} | {} | {}",
                hover.as_deref().unwrap_or("-"),
                target.as_deref().unwrap_or("-")
            ));
        }
        for (needle, offset) in [("image::\n", 7), ("image::sub/", 11)] {
            let labels = completion_labels(server, uri, position_of(text, needle, offset)).await;
            seen.push(format!("complete {needle:?} | {}", labels.join(", ")));
        }
        seen
    }

    /// Verify "B, C, E behave like A and D", part 1.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_fixture_symlinked_paths_behave_like_canonical_paths() {
        let fx = fixture_dir("symlinks");
        let expected = vec![
            "image::logo.svg | image::../assets/logo.svg[]\n\n✓ File exists \
             | <fx>/real/project/assets/logo.svg:0",
            "image::{logo-name} | :logo-name: logo.svg\n\nDefined in ch1.adoc:4 | <self>:3",
            "image::sub/nested | image::../assets/sub/nested.svg[]\n\n✓ File exists \
             | <fx>/real/project/assets/sub/nested.svg:0",
            "image::missing | image::../assets/missing.svg[]\n\n\
             ✗ File not found (imagesdir from ch1.adoc:2) | -",
            "{my_variable} | :my_variable: value\n\nDefined in ch1.adoc:3 | <self>:2",
            "complete \"image::\\n\" | inline.svg, logo.svg, sub/",
            "complete \"image::sub/\" | nested.svg",
        ];
        for (name, root, doc) in ch1_scenarios(&fx) {
            let (service, _socket, uri, text) = open_fixture(&root, &doc).await;

            let observed = observe_ch1(service.inner(), &uri, &text, &fx).await;

            assert_eq!(observed, expected, "scenario {name}");
        }
    }

    /// Verify "B, C, E behave like A and D", part 2.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_fixture_config_imagesdir_through_symlinked_root() {
        let fx = fixture_dir("symlinks");
        let real = fx.join("real/project");
        let linked = fx.join("linked");
        let logo = real.join("assets/logo.svg");
        for (name, root, doc) in [
            ("D", &real, real.join("config/ch2.adoc")),
            ("E", &linked, linked.join("config/ch2.adoc")),
        ] {
            let (service, _socket, uri, text) = open_fixture(root, &doc).await;
            let server = service.inner();
            let position = position_of(&text, "image::logo.svg", 9);

            let hover = hover_text(server, &uri, position).await;
            let target = definition(server, &uri, position).await;

            // The hover shows imagesdir as written in the config, resolved
            // from the document folder (E through `linked`); gd returns the
            // canonical file.
            assert_eq!(
                hover.as_deref(),
                Some("image::../assets/logo.svg[]\n\n✓ File exists"),
                "scenario {name}"
            );
            assert_eq!(
                target.map(|l| l.uri),
                Some(file_uri(&logo)),
                "scenario {name}"
            );
        }
    }

    /// Verify "explainPosition shows raw and canonical path".
    #[cfg(unix)]
    #[tokio::test]
    async fn test_fixture_explain_position_reports_raw_and_canonical_path() {
        let fx = fixture_dir("symlinks");
        let real = fx.join("real/project");
        let linked = fx.join("linked");
        for (name, root, raw, canonical, needle, offset) in [
            (
                "B",
                &linked,
                "docs/ch1.adoc",
                "docs/ch1.adoc",
                "{my_variable}",
                3,
            ),
            (
                "C",
                &real,
                "docs/ch1.adoc",
                "docs/ch1.adoc",
                "{my_variable}",
                3,
            ),
            (
                "E",
                &linked,
                "config/ch2.adoc",
                "config/ch2.adoc",
                "image::logo.svg",
                9,
            ),
        ] {
            let raw = linked.join(raw);
            let canonical = real.join(canonical);
            let (service, _socket, uri, text) = open_fixture(root, &raw).await;

            let report = explain(service.inner(), &uri, position_of(&text, needle, offset)).await;

            let request = &report["request"];
            assert_eq!(request["raw_path"], raw.display().to_string(), "{name}");
            assert_eq!(
                request["canonical_path"],
                canonical.display().to_string(),
                "{name}"
            );
            assert_eq!(
                request["lookup_path"],
                canonical.display().to_string(),
                "{name}"
            );
            assert_eq!(request["paths_differ"], true, "{name}");
            assert_eq!(request["in_documents_raw"], false, "{name}");
            assert_eq!(request["in_documents_canonical"], true, "{name}");
            assert_eq!(
                request["workspace_root"],
                root.display().to_string(),
                "{name}"
            );
        }
    }

    /// verification run: a single symlinked file, inside the
    /// workspace root and from another root.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_fixture_symlinked_file_resolves_attributes() {
        let fx = fixture_dir("symlinks");
        let canonical = fx.join("real/project/docs/ch1.adoc");
        for (root, raw) in [
            (
                fx.join("real/project"),
                fx.join("real/project/docs/link-in.adoc"),
            ),
            (fx.join("other"), fx.join("other/link.adoc")),
        ] {
            let (service, _socket, uri, text) = open_fixture(&root, &raw).await;
            let server = service.inner();
            let position = position_of(&text, "{my_variable}", 3);

            let hover = hover_text(server, &uri, position).await;
            let target = definition(server, &uri, position).await.unwrap();
            let report = explain(server, &uri, position).await;

            let name = raw.display();
            assert_eq!(
                hover.as_deref(),
                Some(":my_variable: value\n\nDefined in ch1.adoc:3"),
                "{name}"
            );
            assert_eq!(target.uri, uri, "{name}");
            assert_eq!(target.range.start.line, 2, "{name}");
            assert_eq!(report["request"]["raw_path"], raw.display().to_string());
            assert_eq!(
                report["request"]["canonical_path"],
                canonical.display().to_string()
            );
            assert_eq!(
                report["resolution"]["scopes"][0]["definition"]["origin"], "document",
                "{name}"
            );
        }
    }

    /// A single symlinked file resolves relative targets against the link's
    /// folder, as Asciidoctor does (`docdir` keeps the symlink).
    /// `other/relative.adoc` links to `real/project/docs/relative.adoc`; both
    /// folders hold a `part.adoc` and a `pic.svg`.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_fixture_symlinked_file_resolves_relative_targets_from_link_folder() {
        let fx = fixture_dir("symlinks");
        let other = fx.join("other");
        let raw = other.join("relative.adoc");
        let (service, _socket, uri, text) = open_fixture(&other, &raw).await;
        let server = service.inner();
        let include = position_of(&text, "include::part", 10);
        let image = position_of(&text, "image::pic", 8);

        let include_target = definition(server, &uri, include).await;
        let image_target = definition(server, &uri, image).await;
        let labels = completion_labels(server, &uri, position_of(&text, "image::\n", 7)).await;
        let report = explain(server, &uri, image).await;

        assert_eq!(
            include_target.map(|l| l.uri),
            Some(file_uri(&other.join("part.adoc")))
        );
        assert_eq!(
            image_target.map(|l| l.uri),
            Some(file_uri(&other.join("pic.svg")))
        );
        assert_eq!(labels, vec!["pic.svg"]);
        assert_eq!(report["request"]["raw_path"], raw.display().to_string());
        assert_eq!(
            report["request"]["canonical_path"],
            fx.join("real/project/docs/relative.adoc")
                .display()
                .to_string()
        );
    }

    /// In a symlinked folder, `..` pops the link's name, as Asciidoctor's
    /// `PathResolver#system_path` does, instead of going to the target's
    /// parent. `real/project/guide` links to `real/outside/guide`; both
    /// parents hold a `part-up.adoc` and an `up.svg`. The image resolves
    /// against the docdir of `diamond.adoc`, which reads the file:
    /// Asciidoctor renders `../up.svg` there, a file next to `real/project`
    /// that does not exist, so goto has no target. The completion lists the
    /// same folder, the parent of the document's `real/project`, not the
    /// parents the preview reads (ADR-010).
    #[cfg(unix)]
    #[tokio::test]
    async fn test_fixture_parent_of_symlinked_folder_is_the_link_side_parent() {
        let fx = fixture_dir("symlinks");
        let real = fx.join("real/project");
        let (service, _socket, uri, text) = open_fixture(&real, &real.join("guide/up.adoc")).await;
        let server = service.inner();
        let include = position_of(&text, "include::../part-up", 13);
        let image = position_of(&text, "image::../up", 11);

        let include_target = definition(server, &uri, include).await;
        let image_target = definition(server, &uri, image).await;
        let labels = completion_labels(server, &uri, position_of(&text, "image::../\n", 10)).await;

        assert_eq!(
            include_target.map(|l| l.uri),
            Some(file_uri(&real.join("part-up.adoc")))
        );
        assert_eq!(image_target.map(|l| l.uri), None);
        assert!(labels.contains(&"project/".to_string()), "{labels:?}");
        assert!(!labels.contains(&"docs/".to_string()), "{labels:?}");
        assert!(!labels.contains(&"shared/".to_string()), "{labels:?}");
    }

    /// A symlink inside the workspace whose target lies outside takes the
    /// `.asciidoctorconfig` chain of the link's folder, as the `IntelliJ`
    /// plugin does: `config/outside.adoc` gets `imagesdir` from
    /// `config/.asciidoctorconfig`.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_fixture_symlink_to_outside_target_uses_config_of_link_folder() {
        let fx = fixture_dir("symlinks");
        let real = fx.join("real/project");
        let logo = real.join("assets/logo.svg");
        let (service, _socket, uri, text) =
            open_fixture(&real, &real.join("config/outside.adoc")).await;
        let server = service.inner();
        let position = position_of(&text, "image::logo.svg", 9);

        let hover = hover_text(server, &uri, position).await;
        let target = definition(server, &uri, position).await;

        assert_eq!(
            hover.as_deref(),
            Some("image::../assets/logo.svg[]\n\n✓ File exists")
        );
        assert_eq!(target.map(|l| l.uri), Some(file_uri(&logo)));
    }

    /// A symlink outside the workspace whose target lies inside gets no
    /// `.asciidoctorconfig` chain: the walk up from the link's folder never
    /// reaches the root. `other/ch2.adoc` links to `config/ch2.adoc`, whose
    /// folder config sets `imagesdir`.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_fixture_symlink_from_outside_the_root_gets_no_config_chain() {
        let fx = fixture_dir("symlinks");
        let real = fx.join("real/project");
        let (service, _socket, uri, text) = open_fixture(&real, &fx.join("other/ch2.adoc")).await;
        let server = service.inner();
        let position = position_of(&text, "image::logo.svg", 9);

        let hover = hover_text(server, &uri, position).await;
        let target = definition(server, &uri, position).await;

        assert_eq!(
            hover.as_deref(),
            Some("image::logo.svg[]\n\n✗ File not found (imagesdir not set in this build)")
        );
        assert_eq!(target, None);
    }

    // `book/twice.adoc` is also reached through `config/book-link -> ../book`,
    // a folder link inside the root. The scan pops folders from a stack of
    // sorted entries, so it visits them in reverse order and reaches the file
    // through `config/book-link` before `book/` (asserted in the core test
    // `test_fixture_scan_reaches_book_through_the_link_first`).
    // `include::../part-up.adoc[]` reaches `part-up.adoc` from the real folder
    // and `config/part-up.adoc` from the link; only the preview of the link
    // side gets `imagesdir` from `config/.asciidoctorconfig`, the build of
    // `via-link.adoc`, which includes the file, does not (ADR-3).

    fn first_include_target(server: &AsciiDocLsp, file: &Path) -> PathBuf {
        let ws = server.workspace.lock();
        ws.as_ref().unwrap().include_graph().includes_of(file)[0]
            .target
            .clone()
    }

    /// Option A: a file inside the root that a folder link inside the root
    /// also reaches resolves from its real folder while unopened (the scan
    /// keeps the canonical folder, though it reaches the file through the
    /// link), and from the link's folder once opened through the link. The
    /// link folder's config chain enters only through the preview scope of
    /// the file (ADR-3); the scope of the including document `via-link.adoc`
    /// does not get it.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_fixture_file_behind_folder_link_resolves_from_real_folder_until_opened_through_it(
    ) {
        let fx = fixture_dir("symlinks");
        let real = fx.join("real/project");
        let canonical = real.join("book/twice.adoc");
        let (service, _socket) = initialized_server(&real).await;
        let server = service.inner();
        let scanned_include = first_include_target(server, &canonical);
        let scanned_imagesdir = {
            let ws = server.workspace.lock();
            let scopes = ws.as_ref().unwrap().scopes_at(&canonical, 1);
            scopes[0]
                .definition("imagesdir")
                .and_then(|definition| definition.value)
        };

        let link = real.join("config/book-link/twice.adoc");
        let text = fs::read_to_string(&link).unwrap();
        let uri = file_uri(&link);
        open_document(server, &uri, &text).await;
        let include = definition(server, &uri, position_of(&text, "include::../part-up", 13)).await;
        let image = hover_text(server, &uri, position_of(&text, "image::logo", 9)).await;

        assert_eq!(scanned_include, real.join("part-up.adoc"));
        assert_eq!(scanned_imagesdir, None);
        assert_eq!(
            include.map(|l| l.uri),
            Some(file_uri(&real.join("config/part-up.adoc")))
        );
        // `via-link.adoc` includes the file, so its build has no `imagesdir`.
        // The preview of the file takes `imagesdir` from the chain of
        // `config/`; its literal `../assets` resolves from the link folder to
        // `config/assets`, as in Asciidoctor (ADR-3).
        assert_eq!(
            image.as_deref(),
            Some(
                "In via-link.adoc:\nimage::logo.svg[]\n\n\
                 ✗ File not found (imagesdir not set in this build)\n\n\
                 In twice.adoc (preview):\nimage::../assets/logo.svg[]\n\n\
                 ✗ File not found (imagesdir from .asciidoctorconfig:1)"
            )
        );
    }

    /// One document directory per file: with the same file open through a
    /// folder link and its real path, the last open, change or save decides
    /// for both buffers.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_fixture_last_open_change_or_save_sets_the_folder_for_both_buffers() {
        let fx = fixture_dir("symlinks");
        let real = fx.join("real/project");
        let real_uri = file_uri(&real.join("book/twice.adoc"));
        let link_uri = file_uri(&real.join("config/book-link/twice.adoc"));
        let text = fs::read_to_string(real.join("book/twice.adoc")).unwrap();
        let include = position_of(&text, "include::../part-up", 13);
        let real_side = Some(file_uri(&real.join("part-up.adoc")));
        let link_side = Some(file_uri(&real.join("config/part-up.adoc")));
        let (service, _socket) = initialized_server(&real).await;
        let server = service.inner();

        open_document(server, &real_uri, &text).await;
        open_document(server, &link_uri, &text).await;
        let after_open = definition(server, &real_uri, include).await.map(|l| l.uri);
        change_document(server, &real_uri, 2, &text).await;
        let after_change = definition(server, &link_uri, include).await.map(|l| l.uri);
        server
            .did_save(DidSaveTextDocumentParams {
                text_document: document_id(&link_uri),
                text: None,
            })
            .await;
        let after_save = definition(server, &real_uri, include).await.map(|l| l.uri);

        assert_eq!(after_open, link_side, "open through the link");
        assert_eq!(after_change, real_side, "change through the real path");
        assert_eq!(after_save, link_side, "save through the link");
    }

    /// Closing a buffer opened through a folder link keeps the link's folder:
    /// the file is re-indexed from disk with the link-side resolution.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_fixture_close_keeps_the_folder_of_the_link() {
        let fx = fixture_dir("symlinks");
        let real = fx.join("real/project");
        let (service, _socket, uri, _) =
            open_fixture(&real, &real.join("config/book-link/twice.adoc")).await;
        let server = service.inner();

        server
            .did_close(DidCloseTextDocumentParams {
                text_document: document_id(&uri),
            })
            .await;

        assert_eq!(
            first_include_target(server, &real.join("book/twice.adoc")),
            real.join("config/part-up.adoc")
        );
    }

    /// Option A for nested includes: opening `via-link.adoc`, which includes
    /// `config/book-link/twice.adoc`, leaves `twice.adoc` on its real folder.
    /// Only opening a file through the link moves it (the CLI walk differs:
    /// there the include path decides).
    #[cfg(unix)]
    #[tokio::test]
    async fn test_fixture_include_through_folder_link_keeps_the_real_folder() {
        let fx = fixture_dir("symlinks");
        let real = fx.join("real/project");
        let twice = real.join("book/twice.adoc");
        let (service, _socket, _, _) = open_fixture(&real, &real.join("via-link.adoc")).await;
        let server = service.inner();

        let include = first_include_target(server, &real.join("via-link.adoc"));
        let nested = first_include_target(server, &twice);

        assert_eq!(include, twice);
        assert_eq!(nested, real.join("part-up.adoc"));
    }

    /// A file outside the root that the scan reaches through several links
    /// takes the folder of the first one in scan order (folders in reverse
    /// sorted order): `docs/outside.adoc` before `config/outside.adoc`, so no
    /// `imagesdir` from `config/.asciidoctorconfig`.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_fixture_scan_takes_the_first_link_to_a_file_outside_the_root() {
        let fx = fixture_dir("symlinks");
        let real = fx.join("real/project");
        let outside = fx.join("real/outside/outside.adoc");
        let (service, _socket) = initialized_server(&real).await;

        let ws = service.inner().workspace.lock();
        let attributes = ws.as_ref().unwrap().attributes();

        assert_eq!(
            attributes.doc_dir(&outside),
            Some(real.join("docs").as_path())
        );
        let scopes = ws.as_ref().unwrap().scopes_at(&outside, 1);
        assert!(scopes
            .iter()
            .all(|scope| scope.value("imagesdir").is_none()));
    }

    /// Verify "inline image has hover and gd like the block macro".
    #[cfg(unix)]
    #[tokio::test]
    async fn test_fixture_inline_image_resolves_like_block_image() {
        let fx = fixture_dir("symlinks");
        let assets = fx.join("real/project/assets");
        for (name, root, doc) in ch1_scenarios(&fx) {
            let (service, _socket, uri, text) = open_fixture(&root, &doc).await;
            let server = service.inner();
            let inline = position_of(&text, "image:inline.svg", 8);
            let block = position_of(&text, "image::logo.svg", 9);

            let inline_hover = hover_text(server, &uri, inline).await;
            let inline_target = definition(server, &uri, inline).await;
            let block_hover = hover_text(server, &uri, block).await;
            let block_target = definition(server, &uri, block).await;

            assert_eq!(
                inline_hover.as_deref(),
                Some("image:../assets/inline.svg[]\n\n✓ File exists"),
                "scenario {name}"
            );
            assert_eq!(
                inline_target.map(|l| l.uri),
                Some(file_uri(&assets.join("inline.svg"))),
                "scenario {name}"
            );
            assert_eq!(
                block_hover.as_deref(),
                Some("image::../assets/logo.svg[]\n\n✓ File exists"),
                "scenario {name}"
            );
            assert_eq!(
                block_target.map(|l| l.uri),
                Some(file_uri(&assets.join("logo.svg"))),
                "scenario {name}"
            );
        }
    }

    /// Verify "remote image: hover shows the URL unchanged, no
    /// File not found".
    #[cfg(unix)]
    #[tokio::test]
    async fn test_fixture_remote_image_is_not_a_local_file() {
        let fx = fixture_dir("symlinks");
        for (name, root, doc) in ch1_scenarios(&fx) {
            let (service, _socket, uri, text) = open_fixture(&root, &doc).await;
            let server = service.inner();
            let position = position_of(&text, "image::https", 9);

            let hover = hover_text(server, &uri, position).await;
            let target = definition(server, &uri, position).await;
            let report = explain(server, &uri, position).await;

            assert_eq!(
                hover.as_deref(),
                Some("image::https://example.com/remote.svg[]"),
                "scenario {name}"
            );
            assert_eq!(target, None, "scenario {name}");
            assert_eq!(
                report["resolution"]["scopes"][0]["remote"], true,
                "scenario {name}"
            );
            assert_eq!(
                report["resolution"]["scopes"][0]["canonical_target"],
                serde_json::Value::Null,
                "scenario {name}"
            );
        }
    }

    /// Verify "report for an image has `canonical_target` equal to
    /// the gd target", over all scenarios A to F.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_fixture_image_report_canonical_target_matches_definition() {
        let fx = fixture_dir("symlinks");
        let real = fx.join("real/project");
        let linked = fx.join("linked");
        let mut cases = Vec::new();
        for (name, root, doc) in ch1_scenarios(&fx) {
            cases.push((name, root.clone(), doc.clone(), "image::logo.svg", 9));
            cases.push((name, root, doc, "image:inline.svg", 8));
        }
        cases.push((
            "D",
            real.clone(),
            real.join("config/ch2.adoc"),
            "image::logo.svg",
            9,
        ));
        cases.push((
            "E",
            linked.clone(),
            linked.join("config/ch2.adoc"),
            "image::logo.svg",
            9,
        ));
        cases.push((
            "F",
            real.clone(),
            real.join("docs/ch3.adoc"),
            "image::linked.svg",
            9,
        ));

        for (name, root, doc, needle, offset) in cases {
            let (service, _socket, uri, text) = open_fixture(&root, &doc).await;
            let server = service.inner();
            let position = position_of(&text, needle, offset);

            let target = definition(server, &uri, position).await.unwrap();
            let report = explain(server, &uri, position).await;

            let target = target.uri.to_file_path().unwrap();
            assert_eq!(
                report["resolution"]["scopes"][0]["canonical_target"],
                target.display().to_string(),
                "scenario {name}, {needle}"
            );
            if name == "F" {
                assert_eq!(target, fx.join("real/outside/shared/linked.svg"));
            }
        }
    }

    /// Applies one LSP edit to an ASCII text (UTF-16 columns equal bytes).
    fn apply_edit(text: &str, edit: &TextEdit) -> String {
        let offset = |position: Position| -> usize {
            let line = usize::try_from(position.line).unwrap();
            let line_start: usize = text.split_inclusive('\n').take(line).map(str::len).sum();
            line_start + usize::try_from(position.character).unwrap()
        };
        let (start, end) = (offset(edit.range.start), offset(edit.range.end));
        format!("{}{}{}", &text[..start], edit.new_text, &text[end..])
    }

    /// Verify "one-sentence-per-line code action joins a wrapped
    /// paragraph", over the protocol with the grammar on main.
    #[tokio::test]
    async fn test_fixture_code_action_puts_one_sentence_per_line() {
        let fx = fixture_dir("ventilate");
        let (service, _socket, uri, text) = open_fixture(&fx, &fx.join("wrapped.adoc")).await;
        let server = service.inner();
        let wrapped = "This is the first sentence of a paragraph that is\n\
                       wrapped over several lines. Here is a second sentence\n\
                       that also wraps. And a third one with *bold text* and\n\
                       a link:https://example.com[link]. Done.\n";
        let ventilated =
            "This is the first sentence of a paragraph that is wrapped over several lines.\n\
                          Here is a second sentence that also wraps.\n\
                          And a third one with *bold text* and a link:https://example.com[link].\n\
                          Done.\n";
        for (needle, before, after) in [
            ("wrapped over", wrapped, ventilated),
            (
                "Some *bold",
                "Some *bold\ntext* here. Two.\n",
                "Some *bold text* here.\nTwo.\n",
            ),
        ] {
            assert!(text.contains(before), "fixture lost {needle:?}");
            let actions = server
                .code_action(CodeActionParams {
                    text_document: tower_lsp_server::ls_types::TextDocumentIdentifier {
                        uri: uri.clone(),
                    },
                    range: Range::new(position_of(&text, needle, 0), position_of(&text, needle, 0)),
                    context: Default::default(),
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                })
                .await
                .unwrap()
                .unwrap();

            let action = actions
                .iter()
                .find_map(|action| match action {
                    CodeActionOrCommand::CodeAction(a) if a.title == "One sentence per line" => {
                        Some(a)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no sentence-per-line action in {actions:?}"));
            assert_eq!(action.kind, Some(CodeActionKind::REFACTOR_REWRITE));
            let edits = &action.edit.as_ref().unwrap().changes.as_ref().unwrap()[&uri];
            let [edit] = edits.as_slice() else {
                panic!("expected one edit, got {edits:?}");
            };
            assert_eq!(
                apply_edit(&text, edit),
                text.replace(before, after),
                "{needle}"
            );
        }
    }

    /// The quick-fix actions the server offers for `range` of `uri`.
    async fn quick_fixes_at(server: &AsciiDocLsp, uri: &Uri, range: Range) -> Vec<CodeAction> {
        server
            .code_action(CodeActionParams {
                text_document: tower_lsp_server::ls_types::TextDocumentIdentifier {
                    uri: uri.clone(),
                },
                range,
                context: Default::default(),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await
            .unwrap()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|action| match action {
                CodeActionOrCommand::CodeAction(a) if a.kind == Some(CodeActionKind::QUICKFIX) => {
                    Some(a)
                }
                _ => None,
            })
            .collect()
    }

    fn only_edit(action: &CodeAction, uri: &Uri) -> TextEdit {
        let edits = &action.edit.as_ref().unwrap().changes.as_ref().unwrap()[uri];
        let [edit] = edits.as_slice() else {
            panic!("expected one edit, got {edits:?}");
        };
        edit.clone()
    }

    /// Asciidoctor (`quick_fixes/main.adoc`): `INFO: possible invalid
    /// reference: setup-instal`.
    #[tokio::test]
    async fn test_fixture_code_action_quick_fixes_a_broken_xref() {
        let fx = fixture_dir("quick_fixes");
        let (service, _socket, uri, text) = open_fixture(&fx, &fx.join("main.adoc")).await;
        let at = position_of(&text, "<<setup-instal>>", 5);

        let fixes = quick_fixes_at(service.inner(), &uri, Range::new(at, at)).await;

        let [fix] = fixes.as_slice() else {
            panic!("expected one quick-fix, got {fixes:?}");
        };
        assert_eq!(fix.title, "Change spelling to 'setup-install'");
        assert_eq!(fix.is_preferred, Some(true));
        let diagnostics = fix.diagnostics.as_ref().unwrap();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].range.start.line, at.line);
        assert_eq!(
            apply_edit(&text, &only_edit(fix, &uri)),
            text.replacen("<<setup-instal>>", "<<setup-install>>", 1)
        );
    }

    #[tokio::test]
    async fn test_fixture_code_action_fixes_only_the_xref_under_the_cursor() {
        let fx = fixture_dir("quick_fixes");
        let (service, _socket, uri, text) = open_fixture(&fx, &fx.join("main.adoc")).await;
        let at = position_of(&text, "Setup-Install", 2);

        let fixes = quick_fixes_at(service.inner(), &uri, Range::new(at, at)).await;

        let [fix] = fixes.as_slice() else {
            panic!("expected one quick-fix, got {fixes:?}");
        };
        let edit = only_edit(fix, &uri);
        assert_eq!(edit.range.start.line, at.line);
        assert!(edit.range.start <= at && at <= edit.range.end);
        assert_eq!(edit.new_text, "setup-install");
    }

    /// A cursor directly before or after the xref still counts as touching it.
    #[tokio::test]
    async fn test_fixture_code_action_quick_fix_at_the_edges_of_the_xref() {
        let fx = fixture_dir("quick_fixes");
        let (service, _socket, uri, text) = open_fixture(&fx, &fx.join("main.adoc")).await;

        for offset in [0, 16] {
            let at = position_of(&text, "<<setup-instal>>", offset);

            let fixes = quick_fixes_at(service.inner(), &uri, Range::new(at, at)).await;

            let [fix] = fixes.as_slice() else {
                panic!("offset {offset}: expected one quick-fix, got {fixes:?}");
            };
            assert_eq!(only_edit(fix, &uri).new_text, "setup-install");
        }
    }

    #[tokio::test]
    async fn test_fixture_code_action_quick_fixes_a_broken_include() {
        let fx = fixture_dir("quick_fixes");
        let (service, _socket, uri, text) = open_fixture(&fx, &fx.join("main.adoc")).await;
        let at = position_of(&text, "include::snippets/config.yaml[]", 3);

        let fixes = quick_fixes_at(service.inner(), &uri, Range::new(at, at)).await;

        let [fix] = fixes.as_slice() else {
            panic!("expected one quick-fix, got {fixes:?}");
        };
        assert_eq!(
            apply_edit(&text, &only_edit(fix, &uri)),
            text.replacen(
                "include::snippets/config.yaml[]",
                "include::snippets/config.yml[]",
                1
            )
        );
    }

    #[tokio::test]
    async fn test_fixture_no_quick_fix_on_a_valid_xref() {
        let fx = fixture_dir("quick_fixes");
        let (service, _socket, uri, text) = open_fixture(&fx, &fx.join("main.adoc")).await;
        let at = position_of(&text, "<<setup-install>>", 4);

        let fixes = quick_fixes_at(service.inner(), &uri, Range::new(at, at)).await;

        assert_eq!(fixes, vec![]);
    }

    #[tokio::test]
    async fn test_code_action_capability_lists_quickfix_and_rewrite() {
        let (service, _socket) = LspService::new(AsciiDocLsp::new);
        let fx = fixture_dir("quick_fixes");

        let result = service
            .inner()
            .initialize(InitializeParams {
                workspace_folders: Some(vec![tower_lsp_server::ls_types::WorkspaceFolder {
                    uri: file_uri(&fx),
                    name: "test".into(),
                }]),
                ..Default::default()
            })
            .await
            .unwrap();

        let Some(CodeActionProviderCapability::Options(options)) =
            result.capabilities.code_action_provider
        else {
            panic!("code action options expected");
        };
        assert_eq!(
            options.code_action_kinds,
            Some(vec![
                CodeActionKind::QUICKFIX,
                CodeActionKind::REFACTOR_REWRITE
            ])
        );
    }

    // --- External file changes (manual test plan T9) ---

    async fn notify_watched(server: &AsciiDocLsp, path: &Path, typ: FileChangeType) {
        server
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: file_uri(path),
                    typ,
                }],
            })
            .await;
    }

    /// The diagnostics the server holds for `path`.
    fn held_diagnostics(
        server: &AsciiDocLsp,
        path: &Path,
    ) -> Vec<asciidoc_core::types::Diagnostic> {
        server
            .diagnostics
            .lock()
            .get(path)
            .cloned()
            .unwrap_or_default()
    }

    /// Targets of the broken xrefs the server holds for `path`.
    fn broken_xrefs(server: &AsciiDocLsp, path: &Path) -> Vec<String> {
        held_diagnostics(server, path)
            .iter()
            .filter_map(|d| match &d.kind {
                asciidoc_core::types::DiagnosticKind::BrokenXref { target, .. } => {
                    Some(target.clone())
                }
                _ => None,
            })
            .collect()
    }

    fn broken_includes(server: &AsciiDocLsp, path: &Path) -> usize {
        held_diagnostics(server, path)
            .iter()
            .filter(|d| {
                matches!(
                    d.kind,
                    asciidoc_core::types::DiagnosticKind::BrokenInclude { .. }
                )
            })
            .count()
    }

    /// T9.1–T9.3: creating, changing and deleting the config changes hover
    /// on an open document without any edit to it.
    #[tokio::test]
    async fn test_config_watch_events_update_hover_without_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let config = root.join(".asciidoctorconfig");
        let text = "= A\n\nGreeting: {greeting}\n";
        fs::write(root.join("a.adoc"), text).unwrap();
        let (service, _socket) = initialized_server(&root).await;
        let server = service.inner();
        let uri = file_uri(&root.join("a.adoc"));
        open_document(server, &uri, text).await;
        let at = position_of(text, "{greeting}", 1);

        let steps = [
            (
                Some(":greeting: hello\n"),
                FileChangeType::CREATED,
                Some("hello"),
            ),
            (
                Some(":greeting: bonjour\n"),
                FileChangeType::CHANGED,
                Some("bonjour"),
            ),
            (None, FileChangeType::DELETED, None),
        ];
        for (content, typ, expected) in steps {
            match content {
                Some(content) => fs::write(&config, content).unwrap(),
                None => fs::remove_file(&config).unwrap(),
            }
            notify_watched(server, &config, typ).await;

            let hover = hover_text(server, &uri, at).await;
            match expected {
                Some(value) => {
                    let hover = hover.unwrap_or_else(|| panic!("no hover after {typ:?}"));
                    assert!(
                        hover.contains(&format!(":greeting: {value}")),
                        "{typ:?}: {hover}"
                    );
                }
                None => assert_eq!(hover, None, "{typ:?}"),
            }
        }
    }

    /// A theme file edited on disk updates the missing-path findings of the
    /// document that names it, without any edit to the document.
    #[tokio::test]
    async fn test_theme_yaml_watch_event_updates_pdf_findings() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let theme = root.join("t-theme.yml");
        let text = "= A\n:pdf-theme: {docdir}/t-theme.yml\n";
        fs::write(
            &theme,
            "title-page:\n  logo:\n    image: image:gone.png[]\n",
        )
        .unwrap();
        fs::write(root.join("a.adoc"), text).unwrap();
        let (service, _socket) = initialized_server(&root).await;
        let server = service.inner();
        open_document(server, &file_uri(&root.join("a.adoc")), text).await;
        let findings = |server: &AsciiDocLsp| {
            held_diagnostics(server, &root.join("a.adoc"))
                .iter()
                .filter(|d| {
                    matches!(
                        d.kind,
                        asciidoc_core::types::DiagnosticKind::PdfThemeEntryNotFound { .. }
                    )
                })
                .count()
        };
        assert_eq!(findings(server), 1);

        fs::write(
            &theme,
            "title-page:\n  logo:\n    image: image:https://e.org/x.png[]\n",
        )
        .unwrap();
        notify_watched(server, &theme, FileChangeType::CHANGED).await;

        assert_eq!(findings(server), 0);
    }

    /// A config event re-reads open documents from the editor, not from disk.
    #[tokio::test]
    async fn test_config_watch_event_keeps_unsaved_buffer_content() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let config = root.join(".asciidoctorconfig");
        fs::write(&config, ":greeting: hello\n").unwrap();
        fs::write(root.join("a.adoc"), "= A\n").unwrap();
        let (service, _socket) = initialized_server(&root).await;
        let server = service.inner();
        let uri = file_uri(&root.join("a.adoc"));
        let unsaved = "= A\n:local: unsaved\n\n{local} {greeting}\n";
        open_document(server, &uri, unsaved).await;

        fs::write(&config, ":greeting: bonjour\n").unwrap();
        notify_watched(server, &config, FileChangeType::CHANGED).await;

        let local = hover_text(server, &uri, position_of(unsaved, "{local}", 1)).await;
        assert!(local.is_some_and(|h| h.contains(":local: unsaved")));
        let greeting = hover_text(server, &uri, position_of(unsaved, "{greeting}", 1)).await;
        assert!(greeting.is_some_and(|h| h.contains(":greeting: bonjour")));
    }

    /// T9.4–T9.6: creating, deleting and editing a closed `.adoc` on disk
    /// updates the broken-xref diagnostics of an open document.
    #[tokio::test]
    async fn test_adoc_watch_events_update_xref_diagnostics() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let a = root.join("a.adoc");
        let b = root.join("b.adoc");
        let text = "= A\n\nSee xref:b.adoc#new-anchor[link].\n";
        fs::write(&a, text).unwrap();
        let (service, _socket) = initialized_server(&root).await;
        let server = service.inner();
        open_document(server, &file_uri(&a), text).await;
        let broken = vec!["b.adoc#new-anchor".to_string()];
        assert_eq!(broken_xrefs(server, &a), broken, "before b.adoc exists");

        fs::write(&b, "[[new-anchor]]\n= B\n").unwrap();
        notify_watched(server, &b, FileChangeType::CREATED).await;
        assert!(broken_xrefs(server, &a).is_empty(), "T9.4: create");

        fs::remove_file(&b).unwrap();
        notify_watched(server, &b, FileChangeType::DELETED).await;
        assert_eq!(broken_xrefs(server, &a), broken, "T9.5: delete");

        fs::write(&b, "[[new-anchor]]\n= B\n").unwrap();
        notify_watched(server, &b, FileChangeType::CREATED).await;
        fs::write(&b, "= B\n").unwrap();
        notify_watched(server, &b, FileChangeType::CHANGED).await;
        assert_eq!(broken_xrefs(server, &a), broken, "T9.6: edit");
    }

    /// Without a watcher event for a new subdirectory file (Neovim's
    /// `watchdirs` fallback), the next edit of the including document
    /// indexes the include target and clears both errors.
    #[tokio::test(start_paused = true)]
    async fn test_edit_indexes_include_target_created_without_watch_event() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("chapters")).unwrap();
        fs::write(root.join("chapters/intro.adoc"), "== Intro\n").unwrap();
        let index = root.join("index.adoc");
        let text = "= I\n\ninclude::chapters/extra.adoc[]\n\nSee <<extra-anchor>>.\n";
        fs::write(&index, text).unwrap();
        let (service, _socket) = initialized_server(&root).await;
        let server = service.inner();
        open_document(server, &file_uri(&index), text).await;
        assert_eq!(broken_xrefs(server, &index), vec!["extra-anchor"]);

        fs::write(
            root.join("chapters/extra.adoc"),
            "[[extra-anchor]]\n== Extra\n",
        )
        .unwrap();
        change_document(server, &file_uri(&index), 2, &format!("{text}\n")).await;
        quiet_period_passes().await;
        assert!(broken_xrefs(server, &index).is_empty());
        assert_eq!(broken_includes(server, &index), 0);
    }

    /// The CREATED event for a subdirectory include target clears both
    /// errors at once, with no edit of the including document; also when the
    /// project is reached through a symlinked root.
    #[tokio::test]
    async fn test_created_event_clears_subdir_include_errors() {
        for symlinked_root in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let base = tmp.path().canonicalize().unwrap();
            let real = base.join("real");
            fs::create_dir_all(real.join("chapters")).unwrap();
            let root = if symlinked_root {
                std::os::unix::fs::symlink(&real, base.join("link")).unwrap();
                base.join("link")
            } else {
                real.clone()
            };
            let text = "= I\n\ninclude::chapters/extra.adoc[]\n\nSee <<extra-anchor>>.\n";
            fs::write(real.join("index.adoc"), text).unwrap();
            let (service, _socket) = initialized_server(&root).await;
            let server = service.inner();
            let index = root.join("index.adoc");
            open_document(server, &file_uri(&index), text).await;
            let key = real.join("index.adoc");
            assert_eq!(broken_xrefs(server, &key), vec!["extra-anchor"]);
            assert_eq!(broken_includes(server, &key), 1);

            let extra = real.join("chapters/extra.adoc");
            fs::write(&extra, "[[extra-anchor]]\n== Extra\n").unwrap();
            notify_watched(server, &extra, FileChangeType::CREATED).await;
            assert!(
                broken_xrefs(server, &key).is_empty(),
                "link={symlinked_root}"
            );
            assert_eq!(broken_includes(server, &key), 0, "link={symlinked_root}");

            fs::remove_file(&extra).unwrap();
            notify_watched(server, &extra, FileChangeType::DELETED).await;
            assert_eq!(broken_xrefs(server, &key), vec!["extra-anchor"]);
            assert_eq!(broken_includes(server, &key), 1);
        }
    }

    /// A missing image is reported until its file appears, and again once
    /// it is deleted; the adoc file is not parsed again for it.
    #[tokio::test]
    async fn test_image_watch_events_update_missing_image_diagnostics() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let doc = root.join("a.adoc");
        let image = root.join("logo.png");
        fs::write(&doc, "= A\n\nimage::logo.png[]\n").unwrap();
        let (service, _socket) = initialized_server(&root).await;
        let server = service.inner();
        assert_eq!(missing_images(server, &doc), 1, "initially missing");

        fs::write(&image, "").unwrap();
        notify_watched(server, &image, FileChangeType::CREATED).await;
        assert_eq!(missing_images(server, &doc), 0, "after create");

        fs::remove_file(&image).unwrap();
        notify_watched(server, &image, FileChangeType::DELETED).await;
        assert_eq!(missing_images(server, &doc), 1, "after delete");
    }

    /// Number of missing-image diagnostics the server holds for `path`.
    fn missing_images(server: &AsciiDocLsp, path: &Path) -> usize {
        held_diagnostics(server, path)
            .iter()
            .filter(|d| {
                matches!(
                    d.kind,
                    asciidoc_core::types::DiagnosticKind::ImageNotFound { .. }
                )
            })
            .count()
    }

    /// T9.7: a watcher event for an open document keeps the editor's
    /// unsaved content.
    #[tokio::test]
    async fn test_adoc_watch_event_for_open_buffer_keeps_unsaved_anchor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let a = root.join("a.adoc");
        let b = root.join("b.adoc");
        let a_text = "= A\n\nSee xref:b.adoc#x[x].\n";
        fs::write(&a, a_text).unwrap();
        fs::write(&b, "= B\n").unwrap();
        let (service, _socket) = initialized_server(&root).await;
        let server = service.inner();
        open_document(server, &file_uri(&a), a_text).await;
        open_document(server, &file_uri(&b), "[[x]]\n= B\n").await;
        assert!(broken_xrefs(server, &a).is_empty(), "unsaved anchor");

        notify_watched(server, &b, FileChangeType::CHANGED).await;

        assert!(broken_xrefs(server, &a).is_empty(), "after event");
    }

    /// gd target of `position` as `<file>:<line>`, the file relative to
    /// `root`; `-` when there is none.
    async fn definition_at(
        server: &AsciiDocLsp,
        uri: &Uri,
        position: Position,
        root: &Path,
    ) -> String {
        definition(server, uri, position).await.map_or_else(
            || "-".to_string(),
            |location| {
                let path = location.uri.to_file_path().unwrap();
                let relative = path.strip_prefix(root).unwrap();
                format!("{}:{}", relative.display(), location.range.start.line)
            },
        )
    }

    /// Navigation keeps working after an attribute entry in a section body
    /// and after a `:to` line without closing colon (committed fixture
    /// `tests/fixtures/attribute_entries`).
    #[tokio::test]
    async fn test_fixture_navigation_after_attribute_entries() {
        let fx = fixture_dir("attribute_entries");
        let (service, _socket, uri, text) = open_fixture(&fx, &fx.join("main.adoc")).await;
        let server = service.inner();
        let product = position_of(&text, "Uses {product}", 6);

        assert_eq!(
            definition_at(server, &uri, position_of(&text, "<<_target", 3), &fx).await,
            "main.adoc:21"
        );
        assert_eq!(
            definition_at(server, &uri, product, &fx).await,
            "main.adoc:19"
        );
        assert_eq!(
            hover_text(server, &uri, product).await.as_deref(),
            Some(":product: Widget\n\nDefined in main.adoc:20")
        );
        assert_eq!(
            definition_at(server, &uri, position_of(&text, "include::part", 10), &fx).await,
            "part.adoc:0"
        );
    }

    /// The committed manual test plan stays navigable past its `:to`
    /// completion probe and the attribute entry in the T10 section body:
    /// T7.12 and T10.1/T10.2.
    #[tokio::test]
    async fn test_manual_test_plan_navigates_after_attribute_entries() {
        let root = workspace_root().join("tests");
        let doc = root.join("manual-test-plan.adoc");
        let (service, _socket, uri, text) = open_fixture(&root, &doc).await;
        let server = service.inner();
        let quality_goals = "chapters/introduction.adoc:8";
        // The plan is edited by hand, so take the definition line from it.
        let product_line = position_of(&text, ":t10-product: Widget", 0).line;
        let product_target = format!("manual-test-plan.adoc:{product_line}");

        for (needle, offset, target) in [
            (
                "| See xref:chapters/introduction.adoc#_quality_goals",
                8,
                quality_goals,
            ),
            (
                "a| See xref:chapters/introduction.adoc#_quality_goals",
                9,
                quality_goals,
            ),
            (
                "image:test-placeholder.png[Placeholder]",
                8,
                "images/test-placeholder.png:0",
            ),
            ("Attribute: {t10-product}", 13, product_target.as_str()),
            ("Xref: xref:chapters/introduction.adoc", 8, quality_goals),
        ] {
            let position = position_of(&text, needle, offset);
            assert_eq!(
                definition_at(server, &uri, position, &root).await,
                target,
                "gd on {needle:?}"
            );
        }
        let image = hover_text(
            server,
            &uri,
            position_of(&text, "image:test-placeholder.png[Placeholder]", 8),
        )
        .await
        .unwrap_or_default();
        assert!(image.ends_with("✓ File exists"), "image hover: {image:?}");
        assert_eq!(
            hover_text(
                server,
                &uri,
                position_of(&text, "Attribute: {t10-product}", 13)
            )
            .await
            .as_deref(),
            Some(format!(
                ":t10-product: Widget\n\nDefined in manual-test-plan.adoc:{}",
                product_line + 1
            ))
            .as_deref()
        );
    }

    /// Navigation keeps working after a section that opens out of sequence,
    /// `===` with no `==` open (committed fixture
    /// `tests/fixtures/out_of_sequence`).
    #[tokio::test]
    async fn test_fixture_navigation_after_out_of_sequence_section() {
        let fx = fixture_dir("out_of_sequence");
        let (service, _socket, uri, text) = open_fixture(&fx, &fx.join("main.adoc")).await;
        let server = service.inner();
        let product = position_of(&text, "Uses {product}", 6);

        assert_eq!(
            definition_at(server, &uri, position_of(&text, "<<_target", 3), &fx).await,
            "main.adoc:13"
        );
        assert_eq!(
            definition_at(server, &uri, position_of(&text, "<<_orphan", 3), &fx).await,
            "main.adoc:7"
        );
        assert_eq!(
            definition_at(server, &uri, product, &fx).await,
            "main.adoc:1"
        );
        assert_eq!(
            hover_text(server, &uri, product).await.as_deref(),
            Some(":product: Widget\n\nDefined in main.adoc:2")
        );
    }

    /// A URI include (literal or via attribute) is never reported as broken,
    /// with or without `allow-uri-read`: the LSP loads nothing over the
    /// network (committed fixtures `tests/fixtures/uri_include_*`).
    #[tokio::test]
    async fn test_fixture_uri_include_is_never_broken() {
        for name in ["uri_include_allowed", "uri_include_default"] {
            let fx = fixture_dir(name);
            let main = fx.join("main.adoc");
            let (service, _socket, _uri, _text) = open_fixture(&fx, &main).await;
            let server = service.inner();

            let broken: Vec<_> = held_diagnostics(server, &main)
                .iter()
                .filter(|d| {
                    matches!(
                        d.kind,
                        asciidoc_core::types::DiagnosticKind::BrokenInclude { .. }
                    )
                })
                .map(|d| d.kind.to_string())
                .collect();
            assert_eq!(
                broken,
                [asciidoc_core::types::DiagnosticKind::BrokenInclude {
                    path: "missing.adoc".into(),
                    documents: Vec::new(),
                }
                .to_string()],
                "{name}"
            );
        }
    }

    /// Hover and explain on a URI include (literal or via attribute) show
    /// the substituted URI as a remote include that is not fetched, never as
    /// a missing file (committed fixture `tests/fixtures/uri_include_default`).
    #[tokio::test]
    async fn test_fixture_uri_include_hover_and_explain_show_remote() {
        let fx = fixture_dir("uri_include_default");
        let main = fx.join("main.adoc");
        let (service, _socket, uri, text) = open_fixture(&fx, &main).await;
        let server = service.inner();

        for (needle, uri_text) in [
            (
                "include::https://example.org/b",
                "https://example.org/b.adoc",
            ),
            ("include::{url-repo}/a", "https://example.org/repo/a.adoc"),
        ] {
            let position = position_of(&text, needle, 4);
            assert_eq!(
                hover_text(server, &uri, position).await,
                Some(format!(
                    "include::{uri_text}[]\n\n\u{2197} Remote include, not fetched"
                )),
                "{needle}"
            );
            let report = explain(server, &uri, position).await;
            let scope = &report["resolution"]["scopes"][0];
            assert_eq!(scope["remote"], true, "{needle}");
            assert_eq!(scope["display"], uri_text, "{needle}");
            assert!(scope["target"].is_null(), "{needle}");
            assert!(scope["exists"].is_null(), "{needle}");
        }
    }

    /// An include or image without its attribute list, as while typing, is
    /// text: no broken include, and navigation after it keeps working
    /// (committed fixture `tests/fixtures/unfinished_block_macro`).
    #[tokio::test]
    async fn test_fixture_unfinished_block_macro_is_text() {
        let fx = fixture_dir("unfinished_block_macro");
        let main = fx.join("main.adoc");
        let (service, _socket, uri, text) = open_fixture(&fx, &main).await;
        let server = service.inner();

        let broken_includes = {
            held_diagnostics(server, &main)
                .iter()
                .filter(|d| {
                    matches!(
                        d.kind,
                        asciidoc_core::types::DiagnosticKind::BrokenInclude { .. }
                    )
                })
                .count()
        };
        assert_eq!(broken_includes, 0);
        assert_eq!(
            definition_at(server, &uri, position_of(&text, "<<_first", 3), &fx).await,
            "main.adoc:6"
        );
        assert_eq!(
            definition_at(server, &uri, position_of(&text, "Uses {product}", 6), &fx).await,
            "main.adoc:1"
        );
    }

    /// Find-references on an xref or an anchor answers the span of the id on
    /// each line, the same spans rename edits (committed fixture
    /// `tests/fixtures/reference_spans`).
    #[tokio::test]
    async fn test_fixture_references_carry_the_spans_rename_edits() {
        let fx = fixture_dir("reference_spans");
        let (service, _socket, uri, text) = open_fixture(&fx, &fx.join("main.adoc")).await;
        let server = service.inner();

        let at_xref = references_of(server, &uri, position_of(&text, "<<target>>", 3)).await;
        let at_anchor = references_of(server, &uri, position_of(&text, "[[target]]", 3)).await;
        let mut spans: Vec<String> = at_xref
            .iter()
            .map(|l| lsp_target(&l.uri, &l.range).replace(fx.to_str().unwrap(), ""))
            .collect();
        spans.sort();

        // Line 11 holds `<<target-old>> ... <<target>>`, line 13 two xrefs to
        // one id.
        let mut expected = vec![
            "file:///main.adoc:4:2-4:8",
            "file:///main.adoc:7:6-7:12",
            "file:///main.adoc:9:16-9:22",
            "file:///main.adoc:11:26-11:32",
            "file:///main.adoc:13:8-13:14",
            "file:///main.adoc:13:23-13:29",
            "file:///other.adoc:3:10-3:16",
        ];
        expected.sort_unstable();
        assert_eq!(spans, expected);
        assert_eq!(at_anchor, at_xref);

        let renamed = rename_edits(server, &uri, position_of(&text, "<<target>>", 3)).await;
        let mut edit_spans: Vec<(Uri, Range)> = renamed
            .into_iter()
            .flat_map(|(uri, edits)| edits.into_iter().map(move |e| (uri.clone(), e.range)))
            .collect();
        let mut ref_spans: Vec<(Uri, Range)> =
            at_xref.into_iter().map(|l| (l.uri, l.range)).collect();
        edit_spans.sort_by_key(|(u, r)| (u.as_str().to_string(), r.start));
        ref_spans.sort_by_key(|(u, r)| (u.as_str().to_string(), r.start));
        assert_eq!(ref_spans, edit_spans);
    }

    /// An anchor with reftext (`[[id,Text]]`) answers the span of its id, and
    /// an implicit section id, which no line spells, stays at the line start.
    #[tokio::test]
    async fn test_fixture_references_reftext_and_implicit_anchors() {
        let fx = fixture_dir("reference_spans");
        let (service, _socket, uri, text) = open_fixture(&fx, &fx.join("main.adoc")).await;
        let server = service.inner();
        let targets = |locations: Vec<Location>| -> Vec<String> {
            let mut t: Vec<String> = locations
                .iter()
                .map(|l| lsp_target(&l.uri, &l.range).replace(fx.to_str().unwrap(), ""))
                .collect();
            t.sort();
            t
        };

        let reftext = references_of(server, &uri, position_of(&text, "<<reftext-id>>", 3)).await;
        assert_eq!(
            targets(reftext),
            vec![
                "file:///main.adoc:15:2-15:12",
                "file:///main.adoc:18:6-18:16"
            ]
        );

        let implicit =
            references_of(server, &uri, position_of(&text, "<<_implicit_section>>", 3)).await;
        assert_eq!(
            targets(implicit),
            vec![
                "file:///main.adoc:20:0-20:0",
                "file:///main.adoc:22:5-22:22"
            ]
        );
    }

    /// Unsaved buffers: the spans come from the edited content, in UTF-16,
    /// for the current buffer and for another open one, and still equal
    /// the rename spans.
    #[tokio::test]
    async fn test_fixture_references_follow_unsaved_buffers() {
        let fx = fixture_dir("reference_spans");
        let (service, _socket, uri, text) = open_fixture(&fx, &fx.join("main.adoc")).await;
        let server = service.inner();
        let other_path = fx.join("other.adoc");
        let other_text = fs::read_to_string(&other_path).unwrap();
        let other_uri = file_uri(&other_path);
        open_document(server, &other_uri, &other_text).await;

        let edited = text.replace("See <<target>>", "See \u{e4}\u{f6} <<target>>");
        change_document(server, &uri, 2, &edited).await;
        let other_edited = other_text.replace("Back to <<target", "Back to \u{e4}\u{f6} <<target");
        change_document(server, &other_uri, 2, &other_edited).await;

        let refs = references_of(server, &uri, position_of(&edited, "<<target>>", 3)).await;
        let mut spans: Vec<(String, u32, u32, u32)> = refs
            .iter()
            .map(|l| {
                (
                    l.uri.as_str().to_string(),
                    l.range.start.line,
                    l.range.start.character,
                    l.range.end.character,
                )
            })
            .collect();
        spans.sort();
        assert!(
            spans
                .iter()
                .any(|s| s.0.ends_with("main.adoc") && s.1 == 7 && s.2 == 9 && s.3 == 15),
            "{spans:?}"
        );
        assert!(
            spans
                .iter()
                .any(|s| s.0.ends_with("other.adoc") && s.1 == 3 && s.2 == 13 && s.3 == 19),
            "{spans:?}"
        );

        let renamed = rename_edits(server, &uri, position_of(&edited, "<<target>>", 3)).await;
        let mut edit_spans: Vec<(Uri, Range)> = renamed
            .into_iter()
            .flat_map(|(uri, edits)| edits.into_iter().map(move |e| (uri.clone(), e.range)))
            .collect();
        let mut ref_spans: Vec<(Uri, Range)> = refs.into_iter().map(|l| (l.uri, l.range)).collect();
        edit_spans.sort_by_key(|(u, r)| (u.as_str().to_string(), r.start));
        ref_spans.sort_by_key(|(u, r)| (u.as_str().to_string(), r.start));
        assert_eq!(ref_spans, edit_spans);
    }

    /// Go-to-definition from an xref lands on the id inside `[[id]]`, or on
    /// the line start when no line spells the id.
    #[tokio::test]
    async fn test_fixture_definition_lands_on_the_anchor_id() {
        let fx = fixture_dir("reference_spans");
        let (service, _socket, uri, text) = open_fixture(&fx, &fx.join("main.adoc")).await;
        let server = service.inner();
        let definition = |position: Position| {
            let params = GotoDefinitionParams {
                text_document_position_params: doc_position(&uri, position),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            async move { server.goto_definition(params).await.unwrap().unwrap() }
        };
        let range_of = |response: GotoDefinitionResponse| match response {
            GotoDefinitionResponse::Scalar(l) => l.range,
            other => panic!("expected one location, got {other:?}"),
        };

        let at_target = range_of(definition(position_of(&text, "<<target>>", 3)).await);
        assert_eq!(
            (at_target.start, at_target.end),
            (Position::new(4, 2), Position::new(4, 8))
        );
        let at_reftext = range_of(definition(position_of(&text, "<<reftext-id>>", 3)).await);
        assert_eq!(
            (at_reftext.start, at_reftext.end),
            (Position::new(15, 2), Position::new(15, 12))
        );
        let at_implicit =
            range_of(definition(position_of(&text, "<<_implicit_section>>", 3)).await);
        assert_eq!(
            (at_implicit.start, at_implicit.end),
            (Position::new(20, 0), Position::new(20, 0))
        );
    }

    /// A cell whose text starts on the line after `a|` and goes on past a
    /// blank line, and a row-span cell after a header gap, parse as cells:
    /// an xref inside the cell and navigation after the tables resolve
    /// (committed fixture `tests/fixtures/table_cells`).
    #[tokio::test]
    async fn test_fixture_table_cells_keep_navigation() {
        let fx = fixture_dir("table_cells");
        let (service, _socket, uri, text) = open_fixture(&fx, &fx.join("main.adoc")).await;
        let server = service.inner();

        assert_eq!(
            definition_at(server, &uri, position_of(&text, "see <<_second", 6), &fx).await,
            "main.adoc:23"
        );
        assert_eq!(
            definition_at(server, &uri, position_of(&text, "and <<_first", 6), &fx).await,
            "main.adoc:3"
        );
        assert_eq!(
            definition_at(server, &uri, position_of(&text, "Uses {product}", 6), &fx).await,
            "main.adoc:1"
        );
    }

    /// The full request answers with the tokens of the open document, as the
    /// legend announced them (committed fixture `tests/fixtures/semantic_tokens`).
    #[tokio::test]
    async fn test_fixture_semantic_tokens_full_returns_tokens_of_the_document() {
        let fx = fixture_dir("semantic_tokens");
        let (service, _socket, uri, text) = open_fixture(&fx, &fx.join("main.adoc")).await;

        let result = service
            .inner()
            .semantic_tokens_full(SemanticTokensParams {
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                text_document: TextDocumentIdentifier { uri },
            })
            .await
            .unwrap();

        let Some(SemanticTokensResult::Tokens(tokens)) = result else {
            panic!("no tokens: {result:?}");
        };
        assert_eq!(
            tokens.data,
            semantic_tokens::semantic_tokens(&text).unwrap()
        );
        let heading = semantic_tokens::legend()
            .token_types
            .iter()
            .position(|t| t.as_str() == "heading")
            .unwrap();
        assert_eq!(tokens.data[0].token_type as usize, heading);
    }

    /// Hover on `pdf-theme`, `pdf-themesdir` and `pdf-fontsdir` shows the
    /// resolved absolute paths, also where the value is set in an included
    /// file (committed fixture `tests/fixtures/pdf_theme`).
    #[tokio::test]
    async fn test_fixture_pdf_attribute_hover_shows_resolved_paths() {
        let fx = fixture_dir("pdf_theme");
        let path = |p: &str| fx.join(p).display().to_string();

        let (service, _socket, uri, text) = open_fixture(&fx, &fx.join("main.adoc")).await;
        let server = service.inner();
        assert_eq!(
            hover_text(server, &uri, position_of(&text, ":pdf-theme:", 1)).await,
            Some(format!(":pdf-theme: {}", path("theme/custom-theme.yml")))
        );
        assert_eq!(
            hover_text(server, &uri, position_of(&text, "{pdf-themesdir}", 2)).await,
            Some(format!(
                ":pdf-themesdir: {}\n\nDefined in pdf-settings.adoc:1",
                path("theme")
            ))
        );

        let shared = fx.join("shared/pdf-settings.adoc");
        let (service, _socket, uri, text) = open_fixture(&fx, &shared).await;
        assert_eq!(
            hover_text(
                service.inner(),
                &uri,
                position_of(&text, ":pdf-fontsdir:", 1)
            )
            .await,
            Some(format!(":pdf-fontsdir: {};GEM_FONTS_DIR", path("fonts")))
        );

        let builtin = fx.join("no-themesdir.adoc");
        let (service, _socket, uri, text) = open_fixture(&fx, &builtin).await;
        assert_eq!(
            hover_text(
                service.inner(),
                &uri,
                position_of(&text, ":pdf-theme: default-sans", 1)
            )
            .await
            .as_deref(),
            Some(":pdf-theme: default-sans (built-in theme)")
        );

        // A relative value cannot be resolved: hover shows it as written.
        let relative = fx.join("relative.adoc");
        let (service, _socket, uri, text) = open_fixture(&fx, &relative).await;
        assert_eq!(
            hover_text(
                service.inner(),
                &uri,
                position_of(&text, ":pdf-themesdir:", 1)
            )
            .await
            .as_deref(),
            Some(":pdf-themesdir: theme")
        );
    }

    /// a `./` value in a `.asciidoctorconfig` stays literal, as
    /// prepended text does in Asciidoctor, and resolves against the folder
    /// of the document that uses it, not the config folder. Hover shows the
    /// literal value and the resolved path; `{asciidoctorconfigdir}` anchors
    /// a value to the config folder (committed fixture
    /// `tests/fixtures/config_relative_values`).
    #[tokio::test]
    async fn test_fixture_config_relative_value_resolves_at_use_site() {
        let fx = fixture_dir("config_relative_values");
        let path = |p: &str| fx.join(p).display().to_string();

        let (service, _socket, uri, text) = open_fixture(&fx, &fx.join("sub/a.adoc")).await;
        let server = service.inner();
        let only_sub = position_of(&text, "image::only-sub", 9);
        assert_eq!(
            hover_text(server, &uri, only_sub).await.as_deref(),
            Some("image::./images/only-sub.png[]\n\n✓ File exists")
        );
        assert_eq!(
            definition(server, &uri, only_sub).await.map(|l| l.uri),
            Some(file_uri(&fx.join("sub/images/only-sub.png")))
        );
        assert_eq!(
            hover_text(server, &uri, position_of(&text, "image::only-root", 9))
                .await
                .as_deref(),
            Some(
                "image::./images/only-root.png[]\n\n\
                 ✗ File not found (imagesdir from .asciidoctorconfig:1)"
            )
        );
        assert_eq!(
            hover_text(server, &uri, position_of(&text, "{imagesdir}", 2)).await,
            Some(format!(
                ":imagesdir: ./images\n\nResolves to {}\n\nDefined in .asciidoctorconfig:1",
                path("sub/images")
            ))
        );
        assert_eq!(
            hover_text(server, &uri, position_of(&text, "{anchored-images}", 2)).await,
            Some(format!(
                ":anchored-images: {}\n\nDefined in .asciidoctorconfig:2",
                path("images")
            ))
        );
    }

    /// "Resolves to" shows only for a reference whose value comes
    /// from a config. A `./` value from the document header, and hover on an
    /// attribute entry, show the value without a resolved path (committed fixture
    /// `tests/fixtures/config_relative_values`, `sub/header.adoc`).
    #[tokio::test]
    async fn test_fixture_header_relative_value_shows_no_resolved_path() {
        let fx = fixture_dir("config_relative_values");

        let (service, _socket, uri, text) = open_fixture(&fx, &fx.join("sub/header.adoc")).await;
        let server = service.inner();
        assert_eq!(
            hover_text(server, &uri, position_of(&text, "{imagesdir}", 2))
                .await
                .as_deref(),
            Some(":imagesdir: ./images\n\nDefined in header.adoc:2")
        );
        assert_eq!(
            hover_text(server, &uri, position_of(&text, ":imagesdir:", 2))
                .await
                .as_deref(),
            Some(":imagesdir: ./images\n\nImages directory")
        );
    }

    /// `imagesdir` and attributes in an image target come from an included
    /// file outside the workspace root (committed fixture
    /// `tests/fixtures/imagesdir_from_include`).
    #[tokio::test]
    async fn test_fixture_imagesdir_from_included_file() {
        let fx = fixture_dir("imagesdir_from_include");
        let root = fx.join("doc");
        let (service, _socket, uri, text) = open_fixture(&root, &root.join("main.adoc")).await;
        let server = service.inner();

        assert_eq!(
            hover_text(server, &uri, position_of(&text, "pic.png", 0)).await,
            Some("image::images/pic.png[]\n\n\u{2713} File exists".to_string())
        );
        assert_eq!(
            definition_at(server, &uri, position_of(&text, "pic.png", 0), &root).await,
            "images/pic.png:0"
        );
        assert_eq!(
            hover_text(server, &uri, position_of(&text, "/ok.png", 1)).await,
            Some("image::images/icons/ok.png[]\n\n\u{2713} File exists".to_string())
        );
    }

    /// `{docdir}` in a file included from a top-level document in a
    /// subfolder is that document's folder: hover, go-to-definition through
    /// it and PDF paths agree (committed fixture `tests/fixtures/docdir_top_level`).
    #[tokio::test]
    async fn test_fixture_docdir_in_included_file_is_top_level_folder() {
        let fx = fixture_dir("docdir_top_level");
        let path = |p: &str| fx.join(p).display().to_string();

        let chapters = fx.join("docs/shared/chapters.adoc");
        let (service, _socket, uri, text) = open_fixture(&fx, &chapters).await;
        let server = service.inner();
        // Each top-level document has its own docdir (ADR-3).
        assert_eq!(
            hover_text(server, &uri, position_of(&text, "{docdir}", 2)).await,
            Some(format!(
                "In handbook.adoc:\n:docdir: {}\n\nIn guide.adoc:\n:docdir: {}",
                path("docs/handbook"),
                path("docs/print/guide")
            ))
        );
        assert_eq!(
            definition_at(server, &uri, position_of(&text, "chapters/intro", 2), &fx).await,
            "docs/handbook/chapters/intro.adoc:0"
        );

        let settings = fx.join("docs/shared/pdf-settings.adoc");
        let (service, _socket, uri, text) = open_fixture(&fx, &settings).await;
        assert_eq!(
            hover_text(
                service.inner(),
                &uri,
                position_of(&text, ":pdf-themesdir:", 1)
            )
            .await,
            Some(format!(":pdf-themesdir: {}", path("docs/theme")))
        );
    }

    #[test]
    fn test_page_click_opens_the_file_at_its_zero_based_line() {
        let chapter = PathBuf::from("/book/chapter.adoc");
        let action = preview::PageAction::Click {
            file: chapter.clone(),
            line: 5,
        };
        let Some(EditorRequest::ShowDocument(params)) =
            editor_request(action, None, &HashMap::new(), true)
        else {
            panic!("a click shows the document");
        };
        assert_eq!(params.uri, Uri::from_file_path(&chapter).unwrap());
        assert_eq!(params.take_focus, Some(true));
        let start = Position::new(4, 0);
        assert_eq!(params.selection, Some(Range::new(start, start)));
    }

    #[test]
    fn test_page_click_addresses_an_open_buffer_by_its_client_uri() {
        let chapter = PathBuf::from("/book/chapter.adoc");
        let client_uri: Uri = "file:///book/./chapter.adoc".parse().unwrap();
        let open = HashMap::from([(chapter.clone(), client_uri.clone())]);
        let action = preview::PageAction::Click {
            file: chapter,
            line: 1,
        };
        let Some(EditorRequest::ShowDocument(params)) = editor_request(action, None, &open, true)
        else {
            panic!("a click shows the document");
        };
        assert_eq!(params.uri, client_uri);
    }

    #[test]
    fn test_page_click_is_dropped_without_show_document_support() {
        let action = preview::PageAction::Click {
            file: PathBuf::from("/book/chapter.adoc"),
            line: 1,
        };
        assert_eq!(editor_request(action, None, &HashMap::new(), false), None);
    }

    #[test]
    fn test_page_scroll_reveals_the_zero_based_line_of_the_active_document() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().canonicalize().unwrap().join("a.adoc");
        fs::write(&file, "").unwrap();
        let active = file_uri(&file);
        let action = preview::PageAction::Scroll {
            file: file.clone(),
            line: 10,
            cursor: true,
        };
        assert_eq!(
            editor_request(action, Some(&active), &HashMap::new(), true),
            Some(EditorRequest::RevealLine(RevealLineParams {
                uri: active,
                line: 9,
                cursor: true
            }))
        );
    }

    #[test]
    fn test_page_scroll_never_switches_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (a, b) = (root.join("a.adoc"), root.join("b.adoc"));
        fs::write(&a, "").unwrap();
        fs::write(&b, "").unwrap();
        let action = || preview::PageAction::Scroll {
            file: b.clone(),
            line: 3,
            cursor: false,
        };
        let active = file_uri(&a);
        assert_eq!(
            editor_request(action(), Some(&active), &HashMap::new(), true),
            None
        );
        assert_eq!(editor_request(action(), None, &HashMap::new(), true), None);
    }

    #[tokio::test]
    async fn test_preview_open_rejects_bad_arguments() {
        let tmp = tempfile::tempdir().unwrap();
        let (service, _socket) = initialized_server(tmp.path()).await;
        let server = service.inner();

        assert!(server.preview_open(Vec::new()).await.is_err());
        assert!(server
            .preview_open(vec![serde_json::json!(5)])
            .await
            .is_err());
        assert!(
            server.preview.server.lock().is_none(),
            "nothing starts on bad input"
        );
    }

    #[tokio::test]
    async fn test_preview_debug_switches_the_log_and_shows_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let a = root.join("a.adoc");
        fs::write(&a, "").unwrap();
        let uri_a = file_uri(&a);
        let (service, _socket) = initialized_server(&root).await;
        let server = service.inner();
        let debug = |action: &str| vec![serde_json::json!(action)];

        assert!(
            server.preview_debug(&debug("on")).is_err(),
            "no preview is open"
        );

        *server.preview.server.lock() = Some(Arc::new(preview::testing::start().await));
        assert!(server.preview_debug(&[]).is_err());
        assert!(server.preview_debug(&debug("loud")).is_err());

        assert_eq!(
            server.preview_debug(&debug("on")).unwrap(),
            Some(serde_json::json!({ "enabled": true }))
        );
        open_document(server, &uri_a, "alpha").await;
        server
            .cursor_moved(CursorMovedParams {
                uri: uri_a.clone(),
                line: 2,
            })
            .await;
        assert_eq!(
            server.preview_debug(&debug("off")).unwrap(),
            Some(serde_json::json!({ "enabled": false }))
        );
        let shown = server.preview_debug(&debug("show")).unwrap().unwrap();
        assert_eq!(shown["enabled"], serde_json::json!(false));
        assert_eq!(
            shown["pairs"],
            serde_json::json!([{
                "seq": 1,
                "sent": {"file": a.to_string_lossy(), "line": 3},
                "page": [],
            }])
        );
    }

    #[tokio::test]
    async fn test_preview_toc_from_the_command_and_the_page_sets_one_mode() {
        use preview::testing::{address, start_with, Stream, TocConverter};
        use preview_converter::{TocInstruction, TocMode};

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let a = root.join("a.adoc");
        fs::write(&a, "").unwrap();
        let uri_a = file_uri(&a);
        let (service, _socket) = initialized_server(&root).await;
        let server = service.inner();
        let toc = |mode: &str| vec![serde_json::json!(mode)];

        let error = server.preview_toc(&toc("left")).unwrap_err();
        assert!(error.message.contains("no preview is open"), "{error:?}");

        let converter = TocConverter::default();
        let seen = Arc::clone(&converter.seen);
        let preview = Arc::new(start_with(converter).await);
        *server.preview.server.lock() = Some(Arc::clone(&preview));
        let (port, token) = address(&preview);
        let mut first = Stream::open(port, &token).await;
        let mut second = Stream::open(port, &token).await;
        for bad in [vec![], toc("top"), vec![serde_json::json!(1)]] {
            let error = server.preview_toc(&bad).unwrap_err();
            assert!(
                error
                    .message
                    .contains("\"auto\", \"off\", \"left\" or \"right\""),
                "{error:?}"
            );
        }
        open_document(server, &uri_a, "alpha").await;
        *server.preview.active.lock() = Some(uri_a.clone());

        assert_eq!(server.preview_toc(&toc("left")).unwrap(), None);
        for stream in [&mut first, &mut second] {
            let seen = stream.until("alpha</p>").await;
            assert!(
                seen.contains("event: toc\ndata: {\"mode\":\"left\"}"),
                "{seen}"
            );
        }
        assert_eq!(
            seen.lock().last().copied(),
            Some(Some(TocInstruction {
                mode: TocMode::Left,
                previous: None,
            }))
        );

        // The page's choice goes the same way, and later conversions keep it.
        let (actions, receiver) = tokio::sync::mpsc::channel(1);
        tokio::spawn(server.clone().forward_page_actions(receiver));
        actions
            .send(preview::PageAction::Toc(TocMode::Right))
            .await
            .unwrap();
        first.until(r#"{"mode":"right"}"#).await;
        first.until("alpha</p>").await;
        change_document(server, &uri_a, 2, "beta").await;
        first.until("beta</p>").await;
        assert_eq!(
            seen.lock().last().copied(),
            Some(Some(TocInstruction {
                mode: TocMode::Right,
                previous: None,
            }))
        );
    }

    #[tokio::test]
    async fn test_preview_follows_cursor_notifications_and_buffer_changes() {
        use preview::testing::{address, start, Stream};

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (a, b) = (root.join("a.adoc"), root.join("b.adoc"));
        fs::write(&a, "").unwrap();
        fs::write(&b, "").unwrap();
        let (uri_a, uri_b) = (file_uri(&a), file_uri(&b));
        let (service, _socket) = initialized_server(&root).await;
        let server = service.inner();

        // Without a preview the notification does nothing.
        server
            .cursor_moved(CursorMovedParams {
                uri: uri_a.clone(),
                line: 0,
            })
            .await;
        assert!(server.preview.active.lock().is_none());

        let preview = start().await;
        let (port, token) = address(&preview);
        *server.preview.server.lock() = Some(Arc::new(preview));
        let mut stream = Stream::open(port, &token).await;

        open_document(server, &uri_a, "alpha").await;
        server
            .cursor_moved(CursorMovedParams {
                uri: uri_a.clone(),
                line: 2,
            })
            .await;
        // LSP line 2 is source line 3.
        let seen = stream.until(r#""line":3}"#).await;
        assert!(
            seen.contains(&format!(r#""file":"{}""#, a.display())),
            "{seen}"
        );
        stream.until("<p>alpha</p>").await;

        change_document(server, &uri_a, 2, "beta").await;
        stream.until("<p>beta</p>").await;

        // Moving the cursor into another buffer switches the page to it.
        open_document(server, &uri_b, "gamma").await;
        server
            .cursor_moved(CursorMovedParams {
                uri: uri_b.clone(),
                line: 0,
            })
            .await;
        stream.until("<p>gamma</p>").await;
        assert_eq!(server.preview.active.lock().as_ref(), Some(&uri_b));
    }

    /// A book with one chapter whose buffer is open and unsaved, shown by a
    /// real Asciidoctor. `None` if Asciidoctor is not installed.
    async fn book_preview(
        root: &Path,
        server: &AsciiDocLsp,
    ) -> Option<(Uri, Uri, preview::testing::Stream)> {
        use preview::testing::{address, Stream};
        if !preview_converter::asciidoctor_available() {
            return None;
        }
        let (book, chapter) = (root.join("book.adoc"), root.join("ch1.adoc"));
        let book_text = "= Book\n\nintro\n\ninclude::ch1.adoc[]\n";
        fs::write(&book, book_text).unwrap();
        fs::write(&chapter, "== One\n\ndisk text\n").unwrap();
        let (book_uri, chapter_uri) = (file_uri(&book), file_uri(&chapter));
        open_document(server, &book_uri, book_text).await;
        open_document(server, &chapter_uri, "== One\n\nunsaved text\n").await;
        let preview = preview::Preview::start(
            preview_converter::AsciidoctorConverter::new().unwrap(),
            std::time::Duration::from_millis(10),
            String::new(),
            None,
        )
        .await
        .unwrap();
        let (port, token) = address(&preview);
        *server.preview.server.lock() = Some(Arc::new(preview));
        Some((book_uri, chapter_uri, Stream::open(port, &token).await))
    }

    #[tokio::test]
    async fn test_preview_of_a_chapter_shows_the_book_with_the_unsaved_buffer() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (service, _socket) = initialized_server(&root).await;
        let server = service.inner();
        let Some((_, chapter_uri, mut stream)) = book_preview(&root, server).await else {
            return;
        };

        server
            .preview_open(vec![serde_json::json!(chapter_uri)])
            .await
            .unwrap();
        let seen = stream.until("unsaved text").await;
        // The converted document is the book, with the chapter's buffer text.
        assert!(
            seen.contains(&format!(r#""file":"{}""#, root.join("book.adoc").display())),
            "{seen}"
        );
        assert!(seen.contains("intro"), "{seen}");
        assert!(!seen.contains("disk text"), "{seen}");
        // A block of the chapter names its file in the table.
        assert!(seen.contains("data-src-0"), "{seen}");
        assert!(seen.contains("ch1.adoc"), "{seen}");

        // Editing the chapter converts the book again.
        change_document(server, &chapter_uri, 2, "== One\n\nnewer text\n").await;
        stream.until("newer text").await;

        // The cursor event names the chapter, not the book.
        server
            .cursor_moved(CursorMovedParams {
                uri: chapter_uri.clone(),
                line: 2,
            })
            .await;
        let seen = stream.until(r#""line":3}"#).await;
        assert!(
            seen.contains(&format!(
                r#"data: {{"file":"{}","line":3}}"#,
                root.join("ch1.adoc").display()
            )),
            "{seen}"
        );
    }

    #[tokio::test]
    async fn test_editing_a_chapter_converts_the_book_while_the_book_is_active() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (service, _socket) = initialized_server(&root).await;
        let server = service.inner();
        let Some((book_uri, chapter_uri, mut stream)) = book_preview(&root, server).await else {
            return;
        };

        server
            .preview_open(vec![serde_json::json!(book_uri)])
            .await
            .unwrap();
        stream.until("unsaved text").await;
        // The book is the active buffer; a change in the chapter still counts.
        change_document(server, &chapter_uri, 2, "== One\n\nedited elsewhere\n").await;
        stream.until("edited elsewhere").await;
    }

    #[tokio::test]
    async fn test_closing_a_chapter_buffer_shows_the_disk_text_again() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (service, _socket) = initialized_server(&root).await;
        let server = service.inner();
        let Some((book_uri, chapter_uri, mut stream)) = book_preview(&root, server).await else {
            return;
        };

        server
            .preview_open(vec![serde_json::json!(book_uri)])
            .await
            .unwrap();
        stream.until("unsaved text").await;
        server
            .did_close(DidCloseTextDocumentParams {
                text_document: document_id(&chapter_uri),
            })
            .await;
        stream.until("disk text").await;
    }

    #[tokio::test]
    async fn test_external_change_to_a_closed_chapter_converts_the_book() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (service, _socket) = initialized_server(&root).await;
        let server = service.inner();
        let Some((book_uri, chapter_uri, mut stream)) = book_preview(&root, server).await else {
            return;
        };

        server
            .did_close(DidCloseTextDocumentParams {
                text_document: document_id(&chapter_uri),
            })
            .await;
        server
            .preview_open(vec![serde_json::json!(book_uri)])
            .await
            .unwrap();
        stream.until("disk text").await;
        fs::write(root.join("ch1.adoc"), "== One\n\nchanged on disk\n").unwrap();
        server
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: chapter_uri,
                    typ: FileChangeType::CHANGED,
                }],
            })
            .await;
        stream.until("changed on disk").await;
    }

    #[tokio::test]
    async fn test_deleting_an_included_chapter_converts_the_book() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (service, _socket) = initialized_server(&root).await;
        let server = service.inner();
        let Some((book_uri, chapter_uri, mut stream)) = book_preview(&root, server).await else {
            return;
        };

        server
            .did_close(DidCloseTextDocumentParams {
                text_document: document_id(&chapter_uri),
            })
            .await;
        server
            .preview_open(vec![serde_json::json!(book_uri)])
            .await
            .unwrap();
        stream.until("disk text").await;
        fs::remove_file(root.join("ch1.adoc")).unwrap();
        server
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: chapter_uri,
                    typ: FileChangeType::DELETED,
                }],
            })
            .await;
        stream.until("Unresolved directive").await;
    }

    #[tokio::test]
    async fn test_recreating_an_included_chapter_converts_the_book() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (service, _socket) = initialized_server(&root).await;
        let server = service.inner();
        let Some((book_uri, chapter_uri, mut stream)) = book_preview(&root, server).await else {
            return;
        };

        server
            .did_close(DidCloseTextDocumentParams {
                text_document: document_id(&chapter_uri),
            })
            .await;
        server
            .preview_open(vec![serde_json::json!(book_uri)])
            .await
            .unwrap();
        stream.until("disk text").await;
        fs::remove_file(root.join("ch1.adoc")).unwrap();
        server
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: chapter_uri.clone(),
                    typ: FileChangeType::DELETED,
                }],
            })
            .await;
        stream.until("Unresolved directive").await;
        fs::write(root.join("ch1.adoc"), "== One\n\nrestored text\n").unwrap();
        server
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: chapter_uri,
                    typ: FileChangeType::CREATED,
                }],
            })
            .await;
        stream.until("restored text").await;
    }

    #[tokio::test]
    async fn test_part_mode_shows_the_chapter_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (service, _socket) = initialized_server(&root).await;
        let server = service.inner();
        let Some((_, chapter_uri, mut stream)) = book_preview(&root, server).await else {
            return;
        };

        server
            .preview_open(vec![
                serde_json::json!(chapter_uri),
                serde_json::json!({ "mode": "part" }),
            ])
            .await
            .unwrap();
        let seen = stream.until("unsaved text").await;
        assert!(
            seen.contains(&format!(r#""file":"{}""#, root.join("ch1.adoc").display())),
            "{seen}"
        );
        assert!(!seen.contains("intro"), "{seen}");
    }

    #[tokio::test]
    async fn test_preview_of_a_document_nobody_includes_is_shown_alone() {
        use preview::testing::{address, start, Stream};

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let lonely = root.join("lonely.adoc");
        fs::write(&lonely, "").unwrap();
        let uri = file_uri(&lonely);
        let (service, _socket) = initialized_server(&root).await;
        let server = service.inner();
        let preview = start().await;
        let (port, token) = address(&preview);
        *server.preview.server.lock() = Some(Arc::new(preview));
        let mut stream = Stream::open(port, &token).await;

        open_document(server, &uri, "alone").await;
        server
            .preview_open(vec![serde_json::json!(uri)])
            .await
            .unwrap();
        let seen = stream.until("<p>alone</p>").await;
        assert!(
            seen.contains(&format!(r#""file":"{}""#, lonely.display())),
            "{seen}"
        );
    }
}
