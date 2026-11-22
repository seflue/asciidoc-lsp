//! The preview server: a small HTTP server inside the LSP process that serves
//! the preview page and pushes converted HTML and the cursor line to it as
//! server-sent events.
//!
//! Safeguards: it binds `127.0.0.1` on a random port, every URL starts with a
//! random token (a wrong token is a 404), and the `Host` header must name that
//! address and port (anything else is a 403, which stops DNS rebinding).
//!
//! HTTP is hand-written on `tokio::net`: the server answers `GET` for five
//! fixed routes and one event stream, so a request parser of a few lines
//! replaces hyper, hyper-util and http-body-util.
//!
//! Routes below `/<token>/`: the page itself, `style.css`, `page.js`,
//! `scroll-map.js`, `morphdom.js`, the Font Awesome CSS and font, and `events` (SSE), and `files/<path>`: an
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
                // it has the HTML.
                self.set_allowed_images(&images);
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
        let shared = Arc::new(Shared {
            token,
            port,
            stylesheet,
            root,
            allowed_images: Mutex::new(HashSet::new()),
            events,
            snapshot: Mutex::new(Snapshot::default()),
            line_counts: Mutex::new(HashMap::new()),
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
            tasks: [accept, pump],
        })
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
}

/// Parse a request head. `None` if it is malformed or has two `Host` headers.
fn parse_request(head: &str) -> Option<Request<'_>> {
    let mut lines = head.split("\r\n");
    let mut parts = lines.next()?.split(' ');
    let method = parts.next()?;
    let target = parts.next()?;
    if !parts.next()?.starts_with("HTTP/") || parts.next().is_some() {
        return None;
    }
    let mut host = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("host") {
                if host.is_some() {
                    return None;
                }
                host = Some(value.trim());
            }
        }
    }
    Some(Request {
        method,
        target,
        host,
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

/// Read up to the blank line that ends the request head. `None` if the
/// client closed first or the head is too long.
async fn read_head(stream: &mut TcpStream) -> io::Result<Option<String>> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            buffer.truncate(end);
            return Ok(Some(String::from_utf8_lossy(&buffer).into_owned()));
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
        301 => "Moved Permanently",
        400 => "Bad Request",
        403 => "Forbidden",
        405 => "Method Not Allowed",
        _ => "Not Found",
    }
}

const COMMON_HEADERS: &str = "Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
                              Referrer-Policy: no-referrer\r\n\
                              Content-Security-Policy: default-src 'none'; script-src 'self'; \
                              style-src 'self' 'unsafe-inline'; font-src 'self'; img-src 'self' data: https:; \
                              connect-src 'self'; base-uri 'none'; form-action 'none'; \
                              frame-ancestors 'none'\r\n";

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
    let Some(head) = head else {
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
    }
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

/// Opening tag of the image list that the converter puts last in its HTML.
const IMAGE_LIST_OPEN: &str = r#"<script type="application/json" id="adoc-image-files">"#;
const IMAGE_LIST_CLOSE: &str = "</script>\n";

/// Cut the image list off the end of `html`: the HTML without it, and the
/// paths it names. The list sits at the very end, so only the last opening
/// tag counts and document text that looks like one cannot take its place.
/// Without a well-formed list, no image is allowed.
fn split_image_list(html: &str) -> (&str, Vec<String>) {
    let Some(start) = html.rfind(IMAGE_LIST_OPEN) else {
        return (html, Vec::new());
    };
    let Some(json) = html[start + IMAGE_LIST_OPEN.len()..].strip_suffix(IMAGE_LIST_CLOSE) else {
        return (html, Vec::new());
    };
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

    /// Converts to `<p>` plus the request text, without Ruby.
    pub(crate) struct EchoConverter;

    impl Convert for EchoConverter {
        async fn convert(&self, request: &ConvertRequest) -> Result<String, ConvertError> {
            Ok(format!("<p>{}</p>", request.text))
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
    use super::testing::{address, get, get_bytes, request, start, start_with_images, Stream};
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
            assert!(response.contains("frame-ancestors 'none'"), "{target}");
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

    #[test]
    fn parse_request_reads_method_target_and_host() {
        let head = "GET /a/b?x=1 HTTP/1.1\r\nhOsT:  localhost:80 \r\nAccept: */*";
        assert_eq!(
            parse_request(head),
            Some(Request {
                method: "GET",
                target: "/a/b?x=1",
                host: Some("localhost:80"),
            })
        );
        assert_eq!(parse_request("GET /"), None);
        assert_eq!(parse_request("GET / HTTP/1.1\r\nHost: a\r\nHost: b"), None);
        assert_eq!(parse_request("GET / HTTP/1.1 extra\r\nHost: a"), None);
    }
}
