//! The preview server: a small HTTP server inside the LSP process that serves
//! the preview page and pushes converted HTML and the cursor line to it as
//! server-sent events.
//!
//! Safeguards: it binds `127.0.0.1` on a random port, every URL starts with a
//! random token (a wrong token is a 404), and the `Host` header must name that
//! address and port (anything else is a 403, which stops DNS rebinding).
//!
//! HTTP is hand-written on `tokio::net`: the server answers `GET` for five
//! fixed routes and one event stream, and `POST` for the two page actions,
//! so a request parser of a few lines replaces hyper, hyper-util and
//! http-body-util.
//!
//! Routes below `/<token>/`: the page itself, `style.css`, `page.js`,
//! `scroll-map.js`, `page-sync.js`, `morphdom.js`, the Font Awesome CSS and font, and `events` (SSE), and `files/<path>`: an
//! image file below the workspace root that the shown document names. The
//! converter lists those images at the end of its HTML; the server resolves
//! each to a canonical path and serves a request only if its own canonical
//! path is in that list and has an image extension, so `..`, symlinks and
//! files the document does not use stay out.
//!
//! Events, each with one line of JSON as data:
//! - `html`: `{file, lineCount, html}`, a finished conversion of `file`: the
//!   top-level document in whole-document mode, else the active file. The
//!   HTML ends with the file table of its includes; the image list is cut
//!   off before the event goes out.
//! - `cursor`: `{file, line}`; `file` is the file the editor is in, which can
//!   be one that `file` includes, and `line` a 1-based source line of it, see
//!   [`Preview::set_cursor`].
//! - `convert-error`: `{message}`, the conversion failed.
//!
//! Page actions, `POST` with a JSON body `{file, line}` (`line` 1-based, as
//! in `data-line-N`; `file` an absolute path, as in the file table):
//! - `click`: the reader double-clicked a block of `file`.
//! - `scroll`: the reader scrolled, and `line` of `file` is on the page's
//!   reading line; with `"cursor": true` the editor's cursor goes there too.
//!
//! Besides host and token, a page action needs the page's own `Origin`
//! (403 otherwise), so another site in the same browser cannot send one, and
//! `file` must be part of the shown document: the shown file or an entry of
//! its file table (400 otherwise). Accepted actions answer 204 and come out
//! of [`Preview::take_actions`].

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::preview_converter::{self, Convert, ConvertOutcome, ConvertRequest};

const PAGE: &str = include_str!("../assets/preview/index.html");
const PAGE_JS: &str = include_str!("../assets/preview/page.js");
const SCROLL_MAP_JS: &str = include_str!("../assets/preview/scroll-map.js");
const PAGE_SYNC_JS: &str = include_str!("../assets/preview/page-sync.js");
const MORPHDOM_JS: &str = include_str!("../assets/preview/morphdom.js");
/// Font Awesome 4.7.0 for `:icons: font`; the standalone page links it from a CDN.
const FONT_AWESOME_CSS: &str = include_str!("../assets/preview/font-awesome.min.css");
const FONT_AWESOME_WOFF2: &[u8] = include_bytes!("../assets/preview/fontawesome-webfont.woff2");

/// Longest request head (request line and headers) the server reads.
const MAX_HEAD_BYTES: usize = 16 * 1024;
/// How long a client may take to send its request head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// Idle time after which the event stream sends a comment line, which also
/// notices a client that went away.
const KEEPALIVE: Duration = Duration::from_secs(15);
/// Buffered events per stream; a slower reader gets the current state again.
const EVENT_BACKLOG: usize = 64;
/// Longest page action body the server reads.
const MAX_BODY_BYTES: usize = 4 * 1024;
/// Page actions waiting for the editor; more are dropped, so a page that
/// floods the server cannot grow the queue.
const ACTION_BACKLOG: usize = 64;

/// Why the server could not start.
#[derive(Debug, thiserror::Error)]
pub enum PreviewError {
    /// The operating system gave no random bytes for the token.
    #[error("no random bytes for the preview token: {0}")]
    Random(getrandom::Error),
    /// The listening socket could not be opened.
    #[error("cannot open the preview server socket: {0}")]
    Bind(io::Error),
}

/// The kind of a server-sent event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventKind {
    Html,
    Cursor,
    ConvertError,
}

impl EventKind {
    /// The event name on the wire.
    fn name(self) -> &'static str {
        match self {
            Self::Html => "html",
            Self::Cursor => "cursor",
            Self::ConvertError => "convert-error",
        }
    }
}

/// One server-sent event.
#[derive(Debug, Clone)]
struct Event {
    kind: EventKind,
    /// One line of JSON.
    data: String,
}

impl Event {
    fn new(kind: EventKind, data: &serde_json::Value) -> Self {
        Self {
            kind,
            data: data.to_string(),
        }
    }

    fn frame(&self) -> String {
        format!("event: {}\ndata: {}\n\n", self.kind.name(), self.data)
    }
}

/// What the page asks the editor to do. Lines are 1-based source lines, as
/// in `data-line-N`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageAction {
    /// The reader double-clicked a block: open `file` at `line`.
    Click { file: PathBuf, line: u32 },
    /// The reader scrolled the page: `line` of `file` is on its reading
    /// line; with `cursor` the editor's cursor goes there too (chapter
    /// changes, start and end of the page).
    Scroll {
        file: PathBuf,
        line: u32,
        cursor: bool,
    },
}

/// Which page action a `POST` route names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionKind {
    Click,
    Scroll,
}

/// The body of a page action.
#[derive(Debug, serde::Deserialize)]
struct ActionBody {
    file: PathBuf,
    line: u32,
    /// Scroll only; absent means `false`.
    #[serde(default)]
    cursor: bool,
}

/// The latest event of each kind, replayed to a stream that connects late.
#[derive(Debug, Default)]
struct Snapshot {
    html: Option<Event>,
    cursor: Option<Event>,
    error: Option<Event>,
}

impl Snapshot {
    fn replay(&self) -> Vec<Event> {
        [&self.html, &self.cursor, &self.error]
            .into_iter()
            .flatten()
            .cloned()
            .collect()
    }
}

#[derive(Debug)]
struct Shared {
    token: String,
    port: u16,
    stylesheet: String,
    /// Workspace root that `files/` serves from.
    root: Option<PathBuf>,
    /// Canonical paths of the images the shown document names; the only
    /// files `files/` serves.
    allowed_images: Mutex<HashSet<PathBuf>>,
    events: broadcast::Sender<Event>,
    snapshot: Mutex<Snapshot>,
    /// Lines of the last text requested per file, for the page's end sentinel.
    line_counts: Mutex<HashMap<PathBuf, usize>>,
    /// The files of the shown document: the converted file and its file
    /// table. Page actions may name these only.
    shown_files: Mutex<HashSet<PathBuf>>,
    /// Where accepted page actions go.
    actions: mpsc::Sender<PageAction>,
}

impl Shared {
    /// Remember `event` as the current one of its kind and send it to every
    /// stream. Both happen under the snapshot lock, so a stream that
    /// subscribes sees each event exactly once, in the replay or live.
    fn publish(&self, event: Event) {
        let mut snapshot = self.snapshot.lock();
        match event.kind {
            EventKind::Html => {
                snapshot.html = Some(event.clone());
                snapshot.error = None;
            }
            EventKind::Cursor => snapshot.cursor = Some(event.clone()),
            EventKind::ConvertError => snapshot.error = Some(event.clone()),
        }
        // No receiver is fine: nobody has the page open.
        let _ = self.events.send(event);
    }

    fn subscribe(&self) -> (broadcast::Receiver<Event>, Vec<Event>) {
        let snapshot = self.snapshot.lock();
        (self.events.subscribe(), snapshot.replay())
    }

    fn publish_outcome(&self, outcome: ConvertOutcome) {
        match outcome.result {
            Ok(html) => {
                let (html, images) = split_image_list(&html);
                // Before the event: the page asks for the images as soon as
                // it has the HTML, and may send an action right after.
                self.set_allowed_images(&images);
                *self.shown_files.lock() = shown_files(&outcome.file, html);
                let line_count = self
                    .line_counts
                    .lock()
                    .get(&outcome.file)
                    .copied()
                    .unwrap_or(0);
                self.publish(Event::new(
                    EventKind::Html,
                    &serde_json::json!({
                        "file": outcome.file.to_string_lossy(),
                        "lineCount": line_count,
                        "html": html,
                    }),
                ));
            }
            Err(error) => self.publish(Event::new(
                EventKind::ConvertError,
                &serde_json::json!({ "message": error.to_string() }),
            )),
        }
    }

    /// Replace the allowlist by the canonical paths of `listed` (paths below
    /// the root, as in the `src`). Entries that do not resolve or lie
    /// outside the canonical root are dropped.
    fn set_allowed_images(&self, listed: &[String]) {
        let Some(root) = self
            .root
            .as_deref()
            .and_then(|root| root.canonicalize().ok())
        else {
            self.allowed_images.lock().clear();
            return;
        };
        let allowed = listed
            .iter()
            .filter_map(|relative| canonical_inside(&root, relative))
            .collect();
        *self.allowed_images.lock() = allowed;
    }

    fn allows_host(&self, host: &str) -> bool {
        let port = self.port;
        host == format!("127.0.0.1:{port}")
            || host.eq_ignore_ascii_case(&format!("localhost:{port}"))
    }

    /// Whether `origin` is the page's own: the scheme plus an allowed host.
    fn allows_origin(&self, origin: &str) -> bool {
        origin
            .strip_prefix("http://")
            .is_some_and(|host| self.allows_host(host))
    }

    /// Accept a page action of `kind` with `body`, or say why not.
    fn accept_action(&self, kind: ActionKind, body: &[u8]) -> Result<(), u16> {
        let body: ActionBody = serde_json::from_slice(body).map_err(|_| 400_u16)?;
        if body.line == 0 || !self.shown_files.lock().contains(&body.file) {
            return Err(400);
        }
        let action = match kind {
            ActionKind::Click => PageAction::Click {
                file: body.file,
                line: body.line,
            },
            ActionKind::Scroll => PageAction::Scroll {
                file: body.file,
                line: body.line,
                cursor: body.cursor,
            },
        };
        // A full queue or a backend that stopped taking actions loses this
        // one; the page needs no answer either way.
        if let Err(error) = self.actions.try_send(action) {
            tracing::debug!(%error, "page action dropped");
        }
        Ok(())
    }
}

/// A running preview server and the converter feeding it.
///
/// Dropping it stops accepting connections and ends the converter worker.
/// Event streams that are already open end with their client.
#[derive(Debug)]
pub struct Preview {
    shared: Arc<Shared>,
    url: String,
    convert: preview_converter::ConvertHandle,
    /// The receiving end of the page actions, until someone takes it.
    actions: Mutex<Option<mpsc::Receiver<PageAction>>>,
    tasks: [JoinHandle<()>; 2],
}

impl Preview {
    /// Start the server on a random local port, with `converter` behind it.
    ///
    /// `stylesheet` is served as `style.css`. Local images of the document
    /// are served from below `root` as `files/<path>`; without a root there
    /// are none.
    ///
    /// # Errors
    /// [`PreviewError`] if no token can be made or the socket cannot be bound.
    pub async fn start<C: Convert>(
        converter: C,
        debounce: Duration,
        stylesheet: String,
        root: Option<PathBuf>,
    ) -> Result<Self, PreviewError> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).map_err(PreviewError::Random)?;
        let token = bytes.iter().fold(String::new(), |mut token, byte| {
            let _ = write!(token, "{byte:02x}");
            token
        });
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(PreviewError::Bind)?;
        let port = listener.local_addr().map_err(PreviewError::Bind)?.port();
        let (events, _) = broadcast::channel(EVENT_BACKLOG);
        let (actions, action_receiver) = mpsc::channel(ACTION_BACKLOG);
        let shared = Arc::new(Shared {
            token,
            port,
            stylesheet,
            root,
            allowed_images: Mutex::new(HashSet::new()),
            events,
            snapshot: Mutex::new(Snapshot::default()),
            line_counts: Mutex::new(HashMap::new()),
            shown_files: Mutex::new(HashSet::new()),
            actions,
        });
        let url = format!("http://127.0.0.1:{port}/{}/", shared.token);
        let (convert, outcomes) = preview_converter::spawn(converter, debounce);
        let accept = tokio::spawn(accept_loop(listener, Arc::clone(&shared)));
        let pump = tokio::spawn(pump_outcomes(
            outcomes,
            convert.latest_generation(),
            Arc::clone(&shared),
        ));
        Ok(Self {
            shared,
            url,
            convert,
            actions: Mutex::new(Some(action_receiver)),
            tasks: [accept, pump],
        })
    }

    /// The page actions the reader triggers, in order. Only the first call
    /// gets them; later calls get `None`.
    pub fn take_actions(&self) -> Option<mpsc::Receiver<PageAction>> {
        self.actions.lock().take()
    }

    /// The URL of the page, token included.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Queue a conversion; the result reaches the page as an `html` event.
    /// A newer request replaces one that has not been converted yet.
    pub fn request_convert(&self, request: ConvertRequest) {
        let line_count = request.text.lines().count().max(1);
        self.shared
            .line_counts
            .lock()
            .insert(request.file.clone(), line_count);
        if !self.convert.request(request) {
            tracing::warn!("preview converter has stopped");
        }
    }

    /// Tell the page the editor's cursor is on `lsp_line` of `file`.
    ///
    /// `lsp_line` is 0-based, as in every LSP position. Asciidoctor's
    /// `data-line-N` classes count from 1, so the event carries
    /// `lsp_line + 1`; the page compares it with the anchors as it is.
    pub fn set_cursor(&self, file: &Path, lsp_line: u32) {
        self.shared.publish(Event::new(
            EventKind::Cursor,
            &serde_json::json!({
                "file": file.to_string_lossy(),
                "line": u64::from(lsp_line) + 1,
            }),
        ));
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn pump_outcomes(
    mut outcomes: mpsc::UnboundedReceiver<ConvertOutcome>,
    latest: Arc<AtomicU64>,
    shared: Arc<Shared>,
) {
    while let Some(outcome) = outcomes.recv().await {
        // A newer request exists: its own outcome is on the way.
        if outcome.is_stale(&latest) {
            continue;
        }
        shared.publish_outcome(outcome);
    }
}

async fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let shared = Arc::clone(&shared);
                tokio::spawn(async move {
                    if let Err(error) = serve_connection(stream, &shared).await {
                        tracing::debug!(%error, "preview connection ended");
                    }
                });
            }
            Err(error) => {
                tracing::warn!(%error, "preview accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// The parts of a request the server looks at.
#[derive(Debug, PartialEq, Eq)]
struct Request<'a> {
    method: &'a str,
    target: &'a str,
    host: Option<&'a str>,
    origin: Option<&'a str>,
    /// `None` without the header or with a value that is no number.
    content_length: Option<usize>,
}

/// Parse a request head. `None` if it is malformed or has two `Host`,
/// `Origin` or `Content-Length` headers.
fn parse_request(head: &str) -> Option<Request<'_>> {
    let mut lines = head.split("\r\n");
    let mut parts = lines.next()?.split(' ');
    let method = parts.next()?;
    let target = parts.next()?;
    if !parts.next()?.starts_with("HTTP/") || parts.next().is_some() {
        return None;
    }
    let mut host = None;
    let mut origin = None;
    let mut content_length = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let slot = if name.eq_ignore_ascii_case("host") {
                &mut host
            } else if name.eq_ignore_ascii_case("origin") {
                &mut origin
            } else if name.eq_ignore_ascii_case("content-length") {
                &mut content_length
            } else {
                continue;
            };
            if slot.is_some() {
                return None;
            }
            *slot = Some(value.trim());
        }
    }
    Some(Request {
        method,
        target,
        host,
        origin,
        content_length: content_length.and_then(|value| value.parse().ok()),
    })
}

/// What to answer.
#[derive(Debug, PartialEq, Eq)]
enum Routed<'a> {
    Body {
        content_type: &'static str,
        body: &'a [u8],
    },
    Redirect(String),
    Events,
    /// A page action; the body is still to be read.
    Action(ActionKind),
    /// A workspace file by its still percent-encoded path below the root.
    File(String),
    Status(u16),
}

fn route<'a>(shared: &'a Shared, request: &Request<'_>) -> Routed<'a> {
    if !request.host.is_some_and(|host| shared.allows_host(host)) {
        return Routed::Status(403);
    }
    let path = request
        .target
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .strip_prefix('/')
        .unwrap_or_default();
    let (token, rest) = path.split_once('/').unwrap_or((path, ""));
    if token != shared.token {
        return Routed::Status(404);
    }
    if request.method == "POST" {
        let kind = match rest {
            "click" => ActionKind::Click,
            "scroll" => ActionKind::Scroll,
            _ => return Routed::Status(405),
        };
        if !request
            .origin
            .is_some_and(|origin| shared.allows_origin(origin))
        {
            return Routed::Status(403);
        }
        return Routed::Action(kind);
    }
    if request.method != "GET" {
        return Routed::Status(405);
    }
    if !path.contains('/') {
        // The page loads its assets by relative URL, so it needs the slash.
        return Routed::Redirect(format!("/{token}/"));
    }
    let body = |content_type, body: &'a str| Routed::Body {
        content_type,
        body: body.as_bytes(),
    };
    match rest {
        "" => body("text/html; charset=utf-8", PAGE),
        "style.css" => body("text/css; charset=utf-8", &shared.stylesheet),
        "page.js" => body("text/javascript; charset=utf-8", PAGE_JS),
        "scroll-map.js" => body("text/javascript; charset=utf-8", SCROLL_MAP_JS),
        "page-sync.js" => body("text/javascript; charset=utf-8", PAGE_SYNC_JS),
        "morphdom.js" => body("text/javascript; charset=utf-8", MORPHDOM_JS),
        "font-awesome.min.css" => body("text/css; charset=utf-8", FONT_AWESOME_CSS),
        "fontawesome-webfont.woff2" => Routed::Body {
            content_type: "font/woff2",
            body: FONT_AWESOME_WOFF2,
        },
        "events" => Routed::Events,
        _ => match rest.strip_prefix("files/") {
            Some(file) => Routed::File(file.to_owned()),
            None => Routed::Status(404),
        },
    }
}

/// Read up to the blank line that ends the request head: the head, and the
/// bytes after it that came in the same reads (the start of a body). `None`
/// if the client closed first or the head is too long.
async fn read_head(stream: &mut TcpStream) -> io::Result<Option<(String, Vec<u8>)>> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let rest = buffer.split_off(end + 4);
            buffer.truncate(end);
            return Ok(Some((String::from_utf8_lossy(&buffer).into_owned(), rest)));
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return Ok(None);
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        301 => "Moved Permanently",
        400 => "Bad Request",
        403 => "Forbidden",
        405 => "Method Not Allowed",
        _ => "Not Found",
    }
}

/// `frame-ancestors` admits only the clients that show the page in an iframe:
/// VS Code's webview, whose frames sit in the workbench (`vscode-file:`).
/// Browsers and JCEF load the URL directly and need no entry. Other sites
/// cannot frame the page without its token.
const COMMON_HEADERS: &str = "Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
                              Referrer-Policy: no-referrer\r\n\
                              Content-Security-Policy: default-src 'none'; script-src 'self'; \
                              style-src 'self' 'unsafe-inline'; font-src 'self'; img-src 'self' data: https:; \
                              connect-src 'self'; base-uri 'none'; form-action 'none'; \
                              frame-ancestors vscode-webview: vscode-file:\r\n";

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    extra_headers: &str,
    body: &[u8],
) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         {COMMON_HEADERS}{extra_headers}Connection: close\r\n\r\n",
        reason(status),
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await
}

async fn serve_connection(mut stream: TcpStream, shared: &Shared) -> io::Result<()> {
    let head = match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut stream)).await {
        Ok(head) => head?,
        Err(_) => None,
    };
    let Some((head, body_start)) = head else {
        return write_response(&mut stream, 400, "text/plain", "", b"bad request").await;
    };
    let Some(request) = parse_request(&head) else {
        return write_response(&mut stream, 400, "text/plain", "", b"bad request").await;
    };
    match route(shared, &request) {
        Routed::Body { content_type, body } => {
            write_response(&mut stream, 200, content_type, "", body).await
        }
        Routed::Redirect(location) => {
            let extra = format!("Location: {location}\r\n");
            write_response(&mut stream, 301, "text/plain", &extra, b"").await
        }
        Routed::Status(status) => {
            write_response(
                &mut stream,
                status,
                "text/plain",
                "",
                reason(status).as_bytes(),
            )
            .await
        }
        Routed::File(path) => match read_workspace_file(shared, &path).await {
            Some((content_type, body)) => {
                write_response(&mut stream, 200, content_type, "", &body).await
            }
            None => {
                write_response(&mut stream, 404, "text/plain", "", reason(404).as_bytes()).await
            }
        },
        Routed::Events => serve_events(stream, shared).await,
        Routed::Action(kind) => {
            let body = match request.content_length {
                Some(length) if length <= MAX_BODY_BYTES => {
                    tokio::time::timeout(HEAD_TIMEOUT, read_body(&mut stream, body_start, length))
                        .await
                        .unwrap_or(Ok(None))?
                }
                _ => None,
            };
            let status = match body {
                Some(body) => shared
                    .accept_action(kind, &body)
                    .map_or_else(|s| s, |()| 204),
                None => 400,
            };
            let text = if status == 204 { "" } else { reason(status) };
            write_response(&mut stream, status, "text/plain", "", text.as_bytes()).await
        }
    }
}

/// Read a body of `length` bytes, of which `start` has arrived already.
/// `None` if the client closes first or sent more than `length`.
async fn read_body(
    stream: &mut TcpStream,
    mut body: Vec<u8>,
    length: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut chunk = [0_u8; 1024];
    while body.len() < length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Ok((body.len() == length).then_some(body))
}

/// Decode `%XX` escapes. `None` for a bad escape or bytes that are no UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'%' {
            let hex = text.get(index + 1..index + 3)?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(byte);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// The content type for a file name, by extension. Only images the browser
/// shows have one; the route serves nothing else.
fn content_type_of(path: &Path) -> Option<&'static str> {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase());
    match extension.as_deref() {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("svg") => Some("image/svg+xml"),
        Some("webp") => Some("image/webp"),
        Some("avif") => Some("image/avif"),
        Some("bmp") => Some("image/bmp"),
        Some("ico") => Some("image/x-icon"),
        _ => None,
    }
}

/// Opening tag of the file table that the converter puts after the blocks.
const FILE_TABLE_OPEN: &str = r#"<script type="application/json" id="adoc-source-files">"#;

/// The files a page showing `html` of `file` may name: `file` and every
/// entry of the file table. Only the last table counts, as for the image
/// list; without a well-formed one, `file` alone.
fn shown_files(file: &Path, html: &str) -> HashSet<PathBuf> {
    let mut files = HashSet::from([file.to_path_buf()]);
    if let Some((_, table, _)) = last_json_script(html, FILE_TABLE_OPEN) {
        match serde_json::from_str::<Vec<PathBuf>>(table) {
            Ok(table) => files.extend(table),
            Err(error) => tracing::warn!(%error, "preview file table is malformed"),
        }
    }
    files
}

/// Opening tag of the image list that the converter puts last in its HTML.
const IMAGE_LIST_OPEN: &str = r#"<script type="application/json" id="adoc-image-files">"#;
/// What follows the image list's `</script>`: it ends the HTML.
const IMAGE_LIST_REST: &str = "\n";

/// The last `<script>` block of `html` that starts with `open`: where it
/// starts, its JSON, and what follows its `</script>`. The converter writes
/// `<\/` for `</` inside the JSON, so the first `</script>` ends it.
fn last_json_script<'a>(html: &'a str, open: &str) -> Option<(usize, &'a str, &'a str)> {
    let start = html.rfind(open)?;
    let json_and_rest = &html[start + open.len()..];
    let end = json_and_rest.find("</script>")?;
    Some((
        start,
        &json_and_rest[..end],
        &json_and_rest[end + "</script>".len()..],
    ))
}

/// Cut the image list off the end of `html`: the HTML without it, and the
/// paths it names. The list sits at the very end, so only the last opening
/// tag counts and document text that looks like one cannot take its place.
/// Without a well-formed list, no image is allowed.
fn split_image_list(html: &str) -> (&str, Vec<String>) {
    let Some((start, json, rest)) = last_json_script(html, IMAGE_LIST_OPEN) else {
        return (html, Vec::new());
    };
    if rest != IMAGE_LIST_REST {
        return (html, Vec::new());
    }
    // The converter wrote `<\/` for `</`, which is the same JSON string.
    let images = serde_json::from_str(json).unwrap_or_else(|error| {
        tracing::warn!(%error, "preview image list is malformed; serving no images");
        Vec::new()
    });
    (&html[..start], images)
}

/// The canonical path of `relative` below the canonical `root`, if it names
/// an existing path inside it. `relative` must be plain names only: no `..`,
/// no absolute path.
fn canonical_inside(root: &Path, relative: &str) -> Option<PathBuf> {
    let relative = Path::new(relative);
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
    {
        return None;
    }
    let path = root.join(relative).canonicalize().ok()?;
    path.starts_with(root).then_some(path)
}

/// The image at `encoded` (a percent-encoded path below the root) and its
/// content type, or `None` if the shown document does not name it.
///
/// The request path is resolved like the listed ones: plain names only, then
/// canonical, with symlinks resolved. It is served only if that canonical
/// path is in the allowlist and itself has an image extension, so a symlink
/// named `logo.png` that points at another kind of file is not an image.
async fn read_workspace_file(shared: &Shared, encoded: &str) -> Option<(&'static str, Vec<u8>)> {
    let root = shared.root.as_ref()?.clone();
    let relative = percent_decode(encoded)?;
    let path = tokio::task::spawn_blocking(move || {
        canonical_inside(&root.canonicalize().ok()?, &relative)
    })
    .await
    .ok()??;
    let content_type = content_type_of(&path)?;
    if !shared.allowed_images.lock().contains(&path) {
        return None;
    }
    tokio::task::spawn_blocking(move || {
        path.metadata()
            .ok()?
            .is_file()
            .then(|| std::fs::read(&path).ok())?
    })
    .await
    .ok()?
    .map(|body| (content_type, body))
}

async fn write_events(stream: &mut TcpStream, events: &[Event]) -> io::Result<()> {
    for event in events {
        stream.write_all(event.frame().as_bytes()).await?;
    }
    stream.flush().await
}

/// The event stream: the current state first, then every new event.
async fn serve_events(mut stream: TcpStream, shared: &Shared) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n{COMMON_HEADERS}\
         Connection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await?;
    let (mut events, replay) = shared.subscribe();
    write_events(&mut stream, &replay).await?;
    loop {
        tokio::select! {
            next = events.recv() => match next {
                Ok(event) => write_events(&mut stream, &[event]).await?,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Resubscribing drops the stale events still buffered for
                    // this receiver; the snapshot matches the new receiver.
                    let replay;
                    (events, replay) = shared.subscribe();
                    write_events(&mut stream, &replay).await?;
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            () = tokio::time::sleep(KEEPALIVE) => {
                stream.write_all(b": keepalive\n\n").await?;
                stream.flush().await?;
            }
        }
    }
}

/// Help for tests that talk to a running server.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::preview_converter::ConvertError;

    /// Converts to `<p>` plus the request text, without Ruby. Every line
    /// `src:<path>` of the text is an included file, listed in the file
    /// table as Ruby's postprocessor lists it.
    pub(crate) struct EchoConverter;

    impl Convert for EchoConverter {
        async fn convert(&self, request: &ConvertRequest) -> Result<String, ConvertError> {
            let files: Vec<&str> = request
                .text
                .lines()
                .filter_map(|line| line.strip_prefix("src:"))
                .collect();
            if files.is_empty() {
                return Ok(format!("<p>{}</p>", request.text));
            }
            Ok(format!(
                "<p>{}</p>\n{FILE_TABLE_OPEN}{}</script>\n",
                request.text,
                serde_json::to_string(&files).unwrap()
            ))
        }
    }

    /// Like [`EchoConverter`], and every line `img:<path>` of the text is an
    /// image the document names, listed as Ruby's postprocessor lists them.
    /// A text with the word `fail` does not convert.
    pub(crate) struct ImageConverter;

    impl Convert for ImageConverter {
        async fn convert(&self, request: &ConvertRequest) -> Result<String, ConvertError> {
            if request.text.contains("fail") {
                return Err(ConvertError::InvalidOutput);
            }
            let images: Vec<&str> = request
                .text
                .lines()
                .filter_map(|line| line.strip_prefix("img:"))
                .collect();
            Ok(format!(
                "<p>{}</p>\n<script type=\"application/json\" id=\"adoc-image-files\">{}</script>\n",
                request.text,
                serde_json::to_string(&images).unwrap()
            ))
        }
    }

    pub(crate) async fn start() -> Preview {
        start_with_root(None).await
    }

    /// A preview with [`ImageConverter`], serving from below `root`.
    pub(crate) async fn start_with_images(root: PathBuf) -> Preview {
        Preview::start(
            ImageConverter,
            Duration::from_millis(10),
            "body{}".to_owned(),
            Some(root),
        )
        .await
        .unwrap()
    }

    /// Like [`start`], serving workspace files from below `root`.
    pub(crate) async fn start_with_root(root: Option<PathBuf>) -> Preview {
        Preview::start(
            EchoConverter,
            Duration::from_millis(10),
            "body{}".to_owned(),
            root,
        )
        .await
        .unwrap()
    }

    /// `(port, token)` of a running preview.
    pub(crate) fn address(preview: &Preview) -> (u16, String) {
        (preview.shared.port, preview.shared.token.clone())
    }

    /// One request, read to the end of the connection: the whole response.
    pub(crate) async fn get(port: u16, target: &str, host: &str) -> String {
        request(port, "GET", target, host).await
    }

    /// A GET read to the end of the connection: head text and body bytes.
    pub(crate) async fn get_bytes(port: u16, target: &str, host: &str) -> (String, Vec<u8>) {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let text = format!("GET {target} HTTP/1.1\r\nHost: {host}\r\n\r\n");
        stream.write_all(text.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let split = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap();
        let head = String::from_utf8_lossy(&response[..split]).into_owned();
        (head, response[split + 4..].to_vec())
    }

    /// A POST of `body` to `/<token>/<route>`, read to the end of the
    /// connection. `origin` is the `Origin` header, left out if `None`.
    pub(crate) async fn post(
        port: u16,
        token: &str,
        route: &str,
        origin: Option<&str>,
        body: &str,
    ) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let origin = origin.map_or_else(String::new, |origin| format!("Origin: {origin}\r\n"));
        let text = format!(
            "POST /{token}/{route} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{origin}\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(text.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    pub(crate) async fn request(port: u16, method: &str, target: &str, host: &str) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let text = format!("{method} {target} HTTP/1.1\r\nHost: {host}\r\n\r\n");
        stream.write_all(text.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    /// An open event stream.
    pub(crate) struct Stream {
        stream: TcpStream,
        received: String,
    }

    impl Stream {
        pub(crate) async fn open(port: u16, token: &str) -> Self {
            let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                .await
                .unwrap();
            let text = format!("GET /{token}/events HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
            stream.write_all(text.as_bytes()).await.unwrap();
            Self {
                stream,
                received: String::new(),
            }
        }

        /// Read until the received text contains `needle` (5 s at most) and
        /// return everything received so far.
        pub(crate) async fn until(&mut self, needle: &str) -> String {
            let read = async {
                let mut chunk = [0_u8; 4096];
                while !self.received.contains(needle) {
                    let count = self.stream.read(&mut chunk).await.unwrap();
                    assert!(
                        count > 0,
                        "stream closed before {needle:?}: {}",
                        self.received
                    );
                    self.received
                        .push_str(&String::from_utf8_lossy(&chunk[..count]));
                }
            };
            tokio::time::timeout(Duration::from_secs(5), read)
                .await
                .unwrap_or_else(|_| panic!("no {needle:?} in: {}", self.received));
            self.received.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{
        address, get, get_bytes, post, request, start, start_with_images, Stream,
    };
    use super::*;

    fn status_line(response: &str) -> &str {
        response.lines().next().unwrap_or_default()
    }

    #[tokio::test]
    async fn wrong_token_is_not_found() {
        let preview = start().await;
        let (port, _) = address(&preview);
        let response = get(port, "/not-the-token/", &format!("127.0.0.1:{port}")).await;
        assert!(status_line(&response).contains("404"), "{response}");
        let response = get(port, "/", &format!("127.0.0.1:{port}")).await;
        assert!(status_line(&response).contains("404"), "{response}");
    }

    #[tokio::test]
    async fn every_response_carries_the_content_security_policy() {
        let preview = start().await;
        let (port, token) = address(&preview);
        let host = format!("127.0.0.1:{port}");
        for target in [
            format!("/{token}/"),
            format!("/{token}/page.js"),
            format!("/{token}/style.css"),
            "/wrong/".to_owned(),
        ] {
            let response = get(port, &target, &host).await;
            assert!(
                response
                    .contains("Content-Security-Policy: default-src 'none'; script-src 'self';"),
                "{target}: {response}"
            );
            assert!(
                response.contains("frame-ancestors vscode-webview: vscode-file:\r\n"),
                "{target}: {response}"
            );
            assert!(
                response.contains("img-src 'self' data: https:; connect-src 'self';"),
                "{target}: {response}"
            );
            assert!(
                response.contains("X-Content-Type-Options: nosniff"),
                "{target}"
            );
        }
    }

    /// A workspace with `images/pic.png` and a file outside it.
    fn image_workspace() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path().canonicalize().unwrap();
        let root = outer.join("ws");
        std::fs::create_dir_all(root.join("images")).unwrap();
        std::fs::write(root.join("images/pic.png"), b"PNG-bytes").unwrap();
        std::fs::write(root.join("images/other.png"), b"other").unwrap();
        std::fs::write(root.join("images/my pic.SVG"), "<svg/>").unwrap();
        std::fs::write(root.join("images/data.bin"), "x").unwrap();
        std::fs::write(root.join("images/PIC.TXT"), "x").unwrap();
        std::fs::write(outer.join("secret.png"), "secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outer.join("secret.png"), root.join("images/link.png")).unwrap();
        (tmp, root)
    }

    /// Convert `text` as the shown document and wait for its html event.
    /// `text` ends in `done` plus `tag`, which marks this conversion.
    async fn show(preview: &Preview, stream: &mut Stream, images: &[&str], tag: &str) {
        let mut text = String::new();
        for image in images {
            text.push_str(&format!("img:{image}\n"));
        }
        text.push_str(&format!("done{tag}"));
        preview.request_convert(ConvertRequest {
            file: PathBuf::from("/doc.adoc"),
            text,
            attributes: Vec::new(),
            overlay: Vec::new(),
        });
        stream.until(&format!("done{tag}</p>")).await;
    }

    async fn file_status(port: u16, token: &str, path: &str) -> String {
        let host = format!("127.0.0.1:{port}");
        let response = get(port, &format!("/{token}/files/{path}"), &host).await;
        assert!(!response.contains("secret"), "{path}: {response}");
        status_line(&response).to_owned()
    }

    #[tokio::test]
    async fn referenced_images_are_served_with_their_type() {
        let (_tmp, root) = image_workspace();
        let preview = start_with_images(root).await;
        let (port, token) = address(&preview);
        let host = format!("127.0.0.1:{port}");
        let mut stream = Stream::open(port, &token).await;
        show(
            &preview,
            &mut stream,
            &["images/pic.png", "images/my pic.SVG"],
            "1",
        )
        .await;
        for (path, content_type) in [
            ("images/pic.png", "image/png"),
            ("images/my%20pic.SVG", "image/svg+xml"),
        ] {
            let response = get(port, &format!("/{token}/files/{path}"), &host).await;
            assert!(status_line(&response).contains("200"), "{path}: {response}");
            assert!(
                response.contains(&format!("Content-Type: {content_type}\r\n")),
                "{path}: {response}"
            );
            assert!(
                response.contains("Content-Security-Policy: default-src 'none';"),
                "{path}"
            );
            assert!(
                response.contains("X-Content-Type-Options: nosniff"),
                "{path}"
            );
        }
        let response = get(port, &format!("/{token}/files/images/pic.png"), &host).await;
        assert!(response.ends_with("PNG-bytes"), "{response}");
        assert!(response.contains("Content-Length: 9\r\n"), "{response}");
    }

    #[tokio::test]
    async fn the_image_list_does_not_reach_the_page() {
        let (_tmp, root) = image_workspace();
        let preview = start_with_images(root).await;
        let (port, token) = address(&preview);
        let mut stream = Stream::open(port, &token).await;
        show(&preview, &mut stream, &["images/pic.png"], "1").await;
        let all = stream.until("done1</p>").await;
        assert!(!all.contains("adoc-image-files"), "{all}");
    }

    #[tokio::test]
    async fn an_unreferenced_image_is_not_found() {
        let (_tmp, root) = image_workspace();
        let preview = start_with_images(root).await;
        let (port, token) = address(&preview);
        assert!(file_status(port, &token, "images/pic.png")
            .await
            .contains("404"));
        let mut stream = Stream::open(port, &token).await;
        show(&preview, &mut stream, &["images/pic.png"], "1").await;
        assert!(file_status(port, &token, "images/pic.png")
            .await
            .contains("200"));
        assert!(file_status(port, &token, "images/other.png")
            .await
            .contains("404"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_referenced_symlink_to_a_non_image_is_not_found() {
        let (_tmp, root) = image_workspace();
        std::fs::write(root.join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(root.join("secret.txt"), root.join("logo.png")).unwrap();
        let preview = start_with_images(root).await;
        let (port, token) = address(&preview);
        let mut stream = Stream::open(port, &token).await;
        show(&preview, &mut stream, &["logo.png", "images/pic.png"], "1").await;
        assert!(file_status(port, &token, "logo.png").await.contains("404"));
        assert!(file_status(port, &token, "images/pic.png")
            .await
            .contains("200"));
    }

    #[tokio::test]
    async fn an_image_removed_from_the_text_is_not_found_after_the_next_conversion() {
        let (_tmp, root) = image_workspace();
        let preview = start_with_images(root).await;
        let (port, token) = address(&preview);
        let mut stream = Stream::open(port, &token).await;
        show(&preview, &mut stream, &["images/pic.png"], "1").await;
        assert!(file_status(port, &token, "images/pic.png")
            .await
            .contains("200"));
        show(&preview, &mut stream, &["images/other.png"], "2").await;
        assert!(file_status(port, &token, "images/pic.png")
            .await
            .contains("404"));
        assert!(file_status(port, &token, "images/other.png")
            .await
            .contains("200"));
    }

    #[tokio::test]
    async fn a_failed_conversion_keeps_the_previous_allowlist() {
        let (_tmp, root) = image_workspace();
        let preview = start_with_images(root).await;
        let (port, token) = address(&preview);
        let mut stream = Stream::open(port, &token).await;
        show(&preview, &mut stream, &["images/pic.png"], "1").await;
        preview.request_convert(ConvertRequest {
            file: PathBuf::from("/doc.adoc"),
            text: "img:images/other.png\nfail".to_owned(),
            attributes: Vec::new(),
            overlay: Vec::new(),
        });
        stream.until("event: convert-error").await;
        assert!(file_status(port, &token, "images/pic.png")
            .await
            .contains("200"));
        assert!(file_status(port, &token, "images/other.png")
            .await
            .contains("404"));
    }

    #[tokio::test]
    async fn the_allowlist_is_set_before_the_html_event_goes_out() {
        let (_tmp, root) = image_workspace();
        let preview = start_with_images(root).await;
        let (port, token) = address(&preview);
        let mut stream = Stream::open(port, &token).await;
        for round in 0..20 {
            let image = if round % 2 == 0 {
                "images/pic.png"
            } else {
                "images/other.png"
            };
            show(&preview, &mut stream, &[image], &round.to_string()).await;
            // Asked for right after the event, as the browser does.
            assert!(
                file_status(port, &token, image).await.contains("200"),
                "round {round}"
            );
        }
    }

    #[tokio::test]
    async fn workspace_files_outside_the_root_or_missing_are_not_found() {
        let (_tmp, root) = image_workspace();
        let preview = start_with_images(root).await;
        let (port, token) = address(&preview);
        let host = format!("127.0.0.1:{port}");
        let mut stream = Stream::open(port, &token).await;
        // Even a document that names them cannot make these servable.
        let named = [
            "../secret.png",
            "images/link.png",
            "images/missing.png",
            "images/data.bin",
            "images/PIC.TXT",
            "images",
            "/etc/passwd",
        ];
        show(&preview, &mut stream, &named, "1").await;
        for path in [
            "../secret.png",
            "%2e%2e/secret.png",
            "images/../../secret.png",
            "images/../images/pic.png",
            "images/link.png",
            "images/missing.png",
            "images/data.bin",
            "images/PIC.TXT",
            "images",
            "images/",
            "",
            "/etc/passwd",
            "%2Fetc/passwd",
        ] {
            let response = get(port, &format!("/{token}/files/{path}"), &host).await;
            assert!(status_line(&response).contains("404"), "{path}: {response}");
            assert!(!response.contains("secret"), "{path}: {response}");
        }
    }

    #[tokio::test]
    async fn workspace_files_keep_host_token_and_method_checks() {
        let (_tmp, root) = image_workspace();
        let preview = start_with_images(root).await;
        let (port, token) = address(&preview);
        let mut stream = Stream::open(port, &token).await;
        show(&preview, &mut stream, &["images/pic.png"], "1").await;
        let host = format!("127.0.0.1:{port}");
        let target = format!("/{token}/files/images/pic.png");
        let response = get(port, &target, "evil.example").await;
        assert!(status_line(&response).contains("403"), "{response}");
        let response = get(port, "/wrong/files/images/pic.png", &host).await;
        assert!(status_line(&response).contains("404"), "{response}");
        let response = request(port, "POST", &target, &host).await;
        assert!(status_line(&response).contains("405"), "{response}");
    }

    #[tokio::test]
    async fn without_a_root_no_files_are_served() {
        let preview = start().await;
        let (port, token) = address(&preview);
        let host = format!("127.0.0.1:{port}");
        let response = get(port, &format!("/{token}/files/images/pic.png"), &host).await;
        assert!(status_line(&response).contains("404"), "{response}");
    }

    #[tokio::test]
    async fn foreign_host_is_forbidden() {
        let preview = start().await;
        let (port, token) = address(&preview);
        for host in [
            "evil.example",
            "127.0.0.1",
            "127.0.0.1:1",
            "localhost.evil:80",
        ] {
            let response = get(port, &format!("/{token}/"), host).await;
            assert!(status_line(&response).contains("403"), "{host}: {response}");
        }
        // The host check comes first: a wrong token is no oracle either.
        let response = get(port, "/wrong/", "evil.example").await;
        assert!(status_line(&response).contains("403"), "{response}");
    }

    #[tokio::test]
    async fn page_and_assets_are_served_for_both_host_names() {
        let preview = start().await;
        let (port, token) = address(&preview);
        for host in [format!("127.0.0.1:{port}"), format!("localhost:{port}")] {
            let response = get(port, &format!("/{token}/"), &host).await;
            assert!(status_line(&response).contains("200"), "{response}");
            assert!(response.contains("Follow cursor"));
        }
        let host = format!("127.0.0.1:{port}");
        for (path, marker) in [
            ("style.css", "body{}"),
            ("page.js", "EventSource"),
            ("scroll-map.js", "AdocScrollMap"),
            ("page-sync.js", "AdocPageSync"),
            ("morphdom.js", "morphdom"),
        ] {
            let response = get(port, &format!("/{token}/{path}"), &host).await;
            assert!(status_line(&response).contains("200"), "{path}: {response}");
            assert!(response.contains(marker), "{path}");
        }
        let response = get(port, &format!("/{token}/nope"), &host).await;
        assert!(status_line(&response).contains("404"), "{response}");
    }

    #[tokio::test]
    async fn font_awesome_is_served_with_its_types() {
        let preview = start().await;
        let (port, token) = address(&preview);
        let host = format!("127.0.0.1:{port}");
        let (head, body) = get_bytes(port, &format!("/{token}/font-awesome.min.css"), &host).await;
        assert!(head.contains("200"), "{head}");
        assert!(
            head.contains("Content-Type: text/css; charset=utf-8"),
            "{head}"
        );
        let css = String::from_utf8(body).unwrap();
        assert!(css.contains("url('fontawesome-webfont.woff2') format('woff2')"));
        assert!(!css.contains("../fonts/"));
        let (head, body) =
            get_bytes(port, &format!("/{token}/fontawesome-webfont.woff2"), &host).await;
        assert!(head.contains("200"), "{head}");
        assert!(head.contains("Content-Type: font/woff2\r\n"), "{head}");
        assert_eq!(
            body,
            include_bytes!("../assets/preview/fontawesome-webfont.woff2")
        );
    }

    #[tokio::test]
    async fn csp_allows_fonts_from_the_server_only() {
        let preview = start().await;
        let (port, token) = address(&preview);
        let response = get(port, &format!("/{token}/"), &format!("127.0.0.1:{port}")).await;
        assert!(response.contains("font-src 'self';"), "{response}");
    }

    #[tokio::test]
    async fn token_without_slash_redirects_and_post_is_refused() {
        let preview = start().await;
        let (port, token) = address(&preview);
        let host = format!("127.0.0.1:{port}");
        let response = get(port, &format!("/{token}"), &host).await;
        assert!(status_line(&response).contains("301"), "{response}");
        assert!(response.contains(&format!("Location: /{token}/")));
        let response = request(port, "POST", &format!("/{token}/"), &host).await;
        assert!(status_line(&response).contains("405"), "{response}");
    }

    #[tokio::test]
    async fn event_stream_needs_the_token_and_host() {
        let preview = start().await;
        let (port, token) = address(&preview);
        let response = get(port, "/wrong/events", &format!("127.0.0.1:{port}")).await;
        assert!(status_line(&response).contains("404"), "{response}");
        let response = get(port, &format!("/{token}/events"), "evil.example").await;
        assert!(status_line(&response).contains("403"), "{response}");
    }

    #[tokio::test]
    async fn conversion_reaches_the_stream_as_html_event() {
        let preview = start().await;
        let (port, token) = address(&preview);
        let mut stream = Stream::open(port, &token).await;
        preview.request_convert(ConvertRequest {
            file: PathBuf::from("/doc.adoc"),
            text: "one\ntwo\nthree\n".to_owned(),
            attributes: Vec::new(),
            overlay: Vec::new(),
        });
        let all = stream.until(r#""lineCount":3}"#).await;
        assert!(all.contains("event: html"), "{all}");
        assert!(all.contains("text/event-stream"));
        assert!(all.contains(r#""file":"/doc.adoc""#), "{all}");
        assert!(all.contains(r#""lineCount":3"#), "{all}");
        assert!(
            all.contains(r#""html":"<p>one\ntwo\nthree\n</p>""#),
            "{all}"
        );
    }

    #[tokio::test]
    async fn cursor_line_is_sent_one_based() {
        let preview = start().await;
        let (port, token) = address(&preview);
        let mut stream = Stream::open(port, &token).await;
        preview.set_cursor(Path::new("/doc.adoc"), 0);
        let all = stream.until(r#""line":1}"#).await;
        assert!(all.contains("event: cursor"), "{all}");
        assert!(
            all.contains(r#"data: {"file":"/doc.adoc","line":1}"#),
            "{all}"
        );
    }

    #[tokio::test]
    async fn late_stream_gets_the_current_state() {
        let preview = start().await;
        let (port, token) = address(&preview);
        let mut first = Stream::open(port, &token).await;
        preview.request_convert(ConvertRequest {
            file: PathBuf::from("/doc.adoc"),
            text: "x".to_owned(),
            attributes: Vec::new(),
            overlay: Vec::new(),
        });
        first.until("</p>").await;
        preview.set_cursor(Path::new("/doc.adoc"), 4);
        first.until(r#""line":5}"#).await;
        let mut late = Stream::open(port, &token).await;
        let replay = late.until(r#""line":5}"#).await;
        assert!(replay.contains("event: html"), "{replay}");
        assert!(replay.contains("<p>x</p>"), "{replay}");
    }

    /// Show `/doc.adoc` with the included files `includes` and return the
    /// preview's page-action receiver.
    async fn show_with_includes(
        preview: &Preview,
        includes: &[&str],
    ) -> mpsc::Receiver<PageAction> {
        let actions = preview.take_actions().expect("actions taken once");
        let (port, token) = address(preview);
        let mut stream = Stream::open(port, &token).await;
        let mut text = String::new();
        for file in includes {
            text.push_str(&format!("src:{file}\n"));
        }
        text.push_str("shown");
        preview.request_convert(ConvertRequest {
            file: PathBuf::from("/doc.adoc"),
            text,
            attributes: Vec::new(),
            overlay: Vec::new(),
        });
        stream.until("shown</p>").await;
        actions
    }

    fn page_origin(port: u16) -> String {
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn a_click_on_the_shown_document_reaches_the_editor() {
        let preview = start().await;
        let mut actions = show_with_includes(&preview, &[]).await;
        let (port, token) = address(&preview);
        let origin = page_origin(port);
        let body = r#"{"file":"/doc.adoc","line":7}"#;
        let response = post(port, &token, "click", Some(&origin), body).await;
        assert!(status_line(&response).contains("204"), "{response}");
        assert_eq!(
            actions.recv().await,
            Some(PageAction::Click {
                file: PathBuf::from("/doc.adoc"),
                line: 7
            })
        );
    }

    #[tokio::test]
    async fn a_click_on_an_included_file_reaches_the_editor() {
        let preview = start().await;
        let mut actions = show_with_includes(&preview, &["/chapter.adoc"]).await;
        let (port, token) = address(&preview);
        let origin = page_origin(port);
        let body = r#"{"file":"/chapter.adoc","line":2}"#;
        let response = post(port, &token, "click", Some(&origin), body).await;
        assert!(status_line(&response).contains("204"), "{response}");
        assert_eq!(
            actions.recv().await,
            Some(PageAction::Click {
                file: PathBuf::from("/chapter.adoc"),
                line: 2
            })
        );
    }

    #[tokio::test]
    async fn a_scroll_reaches_the_editor() {
        let preview = start().await;
        let mut actions = show_with_includes(&preview, &[]).await;
        let (port, token) = address(&preview);
        let origin = format!("http://localhost:{port}");
        let body = r#"{"file":"/doc.adoc","line":12}"#;
        let response = post(port, &token, "scroll", Some(&origin), body).await;
        assert!(status_line(&response).contains("204"), "{response}");
        assert_eq!(
            actions.recv().await,
            Some(PageAction::Scroll {
                file: PathBuf::from("/doc.adoc"),
                line: 12,
                cursor: false
            })
        );
        let body = r#"{"file":"/doc.adoc","line":7,"cursor":true}"#;
        let response = post(port, &token, "scroll", Some(&origin), body).await;
        assert!(status_line(&response).contains("204"), "{response}");
        assert_eq!(
            actions.recv().await,
            Some(PageAction::Scroll {
                file: PathBuf::from("/doc.adoc"),
                line: 7,
                cursor: true
            })
        );
    }

    #[tokio::test]
    async fn a_page_action_from_another_origin_is_forbidden() {
        let preview = start().await;
        let mut actions = show_with_includes(&preview, &[]).await;
        let (port, token) = address(&preview);
        let body = r#"{"file":"/doc.adoc","line":1}"#;
        for route in ["click", "scroll"] {
            for origin in [Some("http://evil.example"), Some("null"), None] {
                let response = post(port, &token, route, origin, body).await;
                assert!(
                    status_line(&response).contains("403"),
                    "{route} {origin:?}: {response}"
                );
            }
        }
        let other_port = format!("http://127.0.0.1:{}", port.wrapping_add(1));
        let response = post(port, &token, "click", Some(&other_port), body).await;
        assert!(status_line(&response).contains("403"), "{response}");
        assert!(actions.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_page_action_needs_the_token() {
        let preview = start().await;
        let mut actions = show_with_includes(&preview, &[]).await;
        let (port, _) = address(&preview);
        let origin = page_origin(port);
        let body = r#"{"file":"/doc.adoc","line":1}"#;
        let response = post(port, "wrong", "click", Some(&origin), body).await;
        assert!(status_line(&response).contains("404"), "{response}");
        assert!(actions.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_page_action_on_a_file_the_page_does_not_show_is_refused() {
        let preview = start().await;
        let mut actions = show_with_includes(&preview, &["/chapter.adoc"]).await;
        let (port, token) = address(&preview);
        let origin = page_origin(port);
        for route in ["click", "scroll"] {
            let body = r#"{"file":"/etc/passwd","line":1}"#;
            let response = post(port, &token, route, Some(&origin), body).await;
            assert!(
                status_line(&response).contains("400"),
                "{route}: {response}"
            );
        }
        assert!(actions.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_malformed_page_action_is_refused() {
        let preview = start().await;
        let mut actions = show_with_includes(&preview, &[]).await;
        let (port, token) = address(&preview);
        let origin = page_origin(port);
        let oversized = format!(
            r#"{{"file":"/doc.adoc","line":1,"x":"{}"}}"#,
            "a".repeat(8192)
        );
        for body in [
            "not json",
            r#"{"file":"/doc.adoc"}"#,
            r#"{"file":"/doc.adoc","line":0}"#,
            r#"{"file":"/doc.adoc","line":-3}"#,
            oversized.as_str(),
        ] {
            let response = post(port, &token, "click", Some(&origin), body).await;
            assert!(
                status_line(&response).contains("400"),
                "{body:.40}: {response}"
            );
        }
        assert!(actions.try_recv().is_err());
    }

    #[tokio::test]
    async fn page_actions_are_refused_before_a_document_is_shown() {
        let preview = start().await;
        let mut actions = preview.take_actions().unwrap();
        let (port, token) = address(&preview);
        let origin = page_origin(port);
        let body = r#"{"file":"/doc.adoc","line":1}"#;
        let response = post(port, &token, "click", Some(&origin), body).await;
        assert!(status_line(&response).contains("400"), "{response}");
        assert!(actions.try_recv().is_err());
    }

    #[tokio::test]
    async fn get_on_a_page_action_route_is_not_found() {
        let preview = start().await;
        let (port, token) = address(&preview);
        let response = get(
            port,
            &format!("/{token}/click"),
            &format!("127.0.0.1:{port}"),
        )
        .await;
        assert!(status_line(&response).contains("404"), "{response}");
    }

    #[test]
    fn parse_request_reads_method_target_and_host() {
        let head = "GET /a/b?x=1 HTTP/1.1\r\nhOsT:  localhost:80 \r\nAccept: */*";
        assert_eq!(
            parse_request(head),
            Some(Request {
                method: "GET",
                target: "/a/b?x=1",
                host: Some("localhost:80"),
                origin: None,
                content_length: None,
            })
        );
        let head = "POST /t/click HTTP/1.1\r\nHost: h\r\nOrigin: http://h\r\nContent-Length: 12";
        let request = parse_request(head).unwrap();
        assert_eq!(request.origin, Some("http://h"));
        assert_eq!(request.content_length, Some(12));
        assert_eq!(
            parse_request("POST / HTTP/1.1\r\nOrigin: a\r\nOrigin: b"),
            None
        );
        assert_eq!(parse_request("GET /"), None);
        assert_eq!(parse_request("GET / HTTP/1.1\r\nHost: a\r\nHost: b"), None);
        assert_eq!(parse_request("GET / HTTP/1.1 extra\r\nHost: a"), None);
    }
}
