//! `AsciiDoc` to HTML for the preview: shells out to Asciidoctor with the
//! source-line extension, debounced, one run at a time, latest request wins.
//!
//! [`spawn`] starts the worker: send [`ConvertRequest`]s through the
//! [`ConvertHandle`], read finished [`ConvertOutcome`]s from the receiver.
//! A request that arrives while a run is under way kills that run. Each
//! request gets a generation number, which its [`ConvertOutcome`] carries; an
//! outcome can still be queued when a newer request arrives, so a consumer
//! drops outcomes for which [`ConvertOutcome::is_stale`] holds.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;

/// The Ruby extension, written next to a run as a file for `-r`.
const EXTENSION: &str = include_str!("../assets/sourceline.rb");

/// Environment variable that names the overlay file for the Ruby extension.
const OVERLAY_ENV: &str = "ASCIIDOC_LSP_OVERLAY";

/// Environment variable that names the workspace root for the Ruby extension:
/// image URLs below it become `files/<path>`.
const ROOT_ENV: &str = "ASCIIDOC_LSP_ROOT";

/// How long the worker waits for further requests before it converts.
pub const DEBOUNCE: Duration = Duration::from_millis(300);

/// Why a conversion produced no HTML.
#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    /// The `asciidoctor` executable is not on the `PATH`.
    #[error("`{program}` not found; install Asciidoctor (for example `gem install asciidoctor`)")]
    AsciidoctorMissing { program: String },
    /// Starting or talking to the process failed.
    #[error("running Asciidoctor failed: {0}")]
    Io(#[from] io::Error),
    /// Asciidoctor exited with an error.
    #[error("Asciidoctor exited with {status}: {stderr}")]
    Failed { status: String, stderr: String },
    /// The output was not UTF-8.
    #[error("Asciidoctor wrote output that is not UTF-8")]
    InvalidOutput,
    /// A standalone page from Asciidoctor held no embedded stylesheet.
    #[error("Asciidoctor's page has no embedded stylesheet")]
    StylesheetMissing,
}

/// One document state to convert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertRequest {
    /// Absolute path of the document: `docfile`, and `-B` is its folder.
    pub file: PathBuf,
    /// The buffer text, which may differ from the file on disk.
    pub text: String,
    /// `-a name=value` pairs, for example from
    /// `AttributeRegistry::config_attributes`.
    pub attributes: Vec<(String, String)>,
    /// Texts of included files that differ from the disk (unsaved buffers),
    /// by absolute path. Asciidoctor reads these instead of the file, with
    /// all include semantics (`leveloffset`, `tags`, `lines`, nesting).
    pub overlay: Vec<(PathBuf, String)>,
}

/// The result for one request.
#[derive(Debug)]
pub struct ConvertOutcome {
    /// The generation of the request, as counted by the [`ConvertHandle`].
    pub generation: u64,
    /// The document the request named.
    pub file: PathBuf,
    /// The embedded HTML, with `data-line-N` / `data-src-K` classes and the
    /// file table in `<script id="adoc-source-files">`, or why not.
    pub result: Result<String, ConvertError>,
}

impl ConvertOutcome {
    /// Whether a newer request than this outcome's has been made, given the
    /// counter from [`ConvertHandle::latest_generation`].
    #[must_use]
    pub fn is_stale(&self, latest: &AtomicU64) -> bool {
        self.generation < latest.load(Ordering::SeqCst)
    }
}

/// Something that converts one request. Dropping the returned future must
/// abort the run: that is how a newer request cancels an older one.
pub trait Convert: Send + Sync + 'static {
    fn convert(
        &self,
        request: &ConvertRequest,
    ) -> impl Future<Output = Result<String, ConvertError>> + Send;
}

/// Sends requests to the worker. The worker ends when the last handle is
/// dropped.
#[derive(Debug, Clone)]
pub struct ConvertHandle {
    requests: mpsc::UnboundedSender<(u64, ConvertRequest)>,
    latest: Arc<AtomicU64>,
}

impl ConvertHandle {
    /// Queue `request`; it replaces any request not yet converted. Returns
    /// `false` if the worker has ended.
    pub fn request(&self, request: ConvertRequest) -> bool {
        let generation = self.latest.fetch_add(1, Ordering::SeqCst) + 1;
        self.requests.send((generation, request)).is_ok()
    }

    /// The generation of the newest request made so far; outcomes with a
    /// smaller generation are stale.
    #[must_use]
    pub fn latest_generation(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.latest)
    }
}

/// Start the worker on the current tokio runtime.
pub fn spawn<C: Convert>(
    converter: C,
    debounce: Duration,
) -> (ConvertHandle, mpsc::UnboundedReceiver<ConvertOutcome>) {
    let (requests, request_rx) = mpsc::unbounded_channel();
    let (outcome_tx, outcomes) = mpsc::unbounded_channel();
    tokio::spawn(run(converter, debounce, request_rx, outcome_tx));
    (
        ConvertHandle {
            requests,
            latest: Arc::new(AtomicU64::new(0)),
        },
        outcomes,
    )
}

async fn run<C: Convert>(
    converter: C,
    debounce: Duration,
    mut requests: mpsc::UnboundedReceiver<(u64, ConvertRequest)>,
    outcomes: mpsc::UnboundedSender<ConvertOutcome>,
) {
    let mut carried = None;
    loop {
        let (mut generation, mut request) = match carried.take() {
            Some(request) => request,
            None => match requests.recv().await {
                Some(request) => request,
                None => return,
            },
        };
        // Wait until requests stop coming.
        loop {
            tokio::select! {
                newer = requests.recv() => match newer {
                    Some(newer) => (generation, request) = newer,
                    None => return,
                },
                () = tokio::time::sleep(debounce) => break,
            }
        }
        let result = {
            let run = converter.convert(&request);
            tokio::pin!(run);
            tokio::select! {
                biased;
                newer = requests.recv() => match newer {
                    // Leaving the select drops `run`, which kills the process.
                    Some(newer) => {
                        carried = Some(newer);
                        continue;
                    }
                    None => return,
                },
                result = &mut run => result,
            }
        };
        // A request that came in as the run finished makes the result stale.
        if let Ok(newer) = requests.try_recv() {
            carried = Some(newer);
            continue;
        }
        let outcome = ConvertOutcome {
            generation,
            file: request.file,
            result,
        };
        if outcomes.send(outcome).is_err() {
            return;
        }
    }
}

/// A file in a private temporary folder that is removed on drop: the
/// extension file for `-r`, or the overlay of one run.
#[derive(Debug)]
struct PrivateFile {
    dir: PathBuf,
    file: PathBuf,
}

/// A new folder in the temp directory that only the current user can enter.
fn private_dir() -> io::Result<PathBuf> {
    let base = std::env::temp_dir();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let mut last_error = io::Error::other("no free folder name");
    for attempt in 0..8 {
        let dir = base.join(format!(
            "asciidoc-lsp-{}-{stamp}-{attempt}",
            std::process::id()
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => last_error = error,
            Err(error) => return Err(error),
        }
    }
    Err(last_error)
}

/// Write `content` to `file` for the current user only.
fn write_private(file: &Path, content: &str) -> io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(file)?.write_all(content.as_bytes())
}

impl PrivateFile {
    /// Write `content` to `name` in a new private folder.
    fn write(name: &str, content: &str) -> io::Result<Self> {
        let dir = private_dir()?;
        let file = dir.join(name);
        if let Err(error) = write_private(&file, content) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(error);
        }
        Ok(Self { dir, file })
    }
}

impl Drop for PrivateFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The overlay of one run as a JSON file `{absolute path: text}`. The
/// extension reads it from the environment variable [`OVERLAY_ENV`], so no
/// path or text reaches the command line or the document's attributes.
fn overlay_file(entries: &[(PathBuf, String)]) -> io::Result<PrivateFile> {
    let map: serde_json::Map<String, serde_json::Value> = entries
        .iter()
        .map(|(path, text)| {
            (
                path.to_string_lossy().into_owned(),
                serde_json::Value::String(text.clone()),
            )
        })
        .collect();
    let json = serde_json::to_string(&map).map_err(io::Error::other)?;
    PrivateFile::write("overlay.json", &json)
}

/// The failure of an Asciidoctor run that ended unsuccessfully.
fn run_failed(output: &std::process::Output) -> ConvertError {
    ConvertError::Failed {
        status: output.status.to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    }
}

/// Converts with the `asciidoctor` executable.
#[derive(Debug)]
pub struct AsciidoctorConverter {
    program: String,
    extension: PrivateFile,
    root: Option<PathBuf>,
}

impl AsciidoctorConverter {
    /// A converter that runs `asciidoctor` from the `PATH`.
    ///
    /// # Errors
    /// [`ConvertError::Io`] if the extension file cannot be written.
    pub fn new() -> Result<Self, ConvertError> {
        Self::with_program("asciidoctor")
    }

    /// A converter that runs `program`.
    ///
    /// # Errors
    /// [`ConvertError::Io`] if the extension file cannot be written.
    pub fn with_program(program: &str) -> Result<Self, ConvertError> {
        Ok(Self {
            program: program.to_owned(),
            extension: PrivateFile::write("sourceline.rb", EXTENSION)?,
            root: None,
        })
    }

    /// Point local image URLs of the output at `files/<path below root>`,
    /// which the preview server serves from `root`.
    #[must_use]
    pub fn with_root(mut self, root: PathBuf) -> Self {
        self.root = Some(root);
        self
    }

    /// Asciidoctor's default stylesheet, taken from the standalone page of an
    /// empty document (the CLI embeds it there), so no gem path is needed.
    ///
    /// # Errors
    /// [`ConvertError::AsciidoctorMissing`] if the program is not found;
    /// other [`ConvertError`]s if the run fails or the page has no stylesheet.
    pub async fn default_stylesheet(&self) -> Result<String, ConvertError> {
        let output = Command::new(&self.program)
            .args(["-o", "-", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|error| self.spawn_error(error))?;
        if !output.status.success() {
            return Err(run_failed(&output));
        }
        let page = String::from_utf8(output.stdout).map_err(|_| ConvertError::InvalidOutput)?;
        extract_stylesheet(&page).ok_or(ConvertError::StylesheetMissing)
    }

    /// A failure to start the program: it is missing, or the start failed.
    fn spawn_error(&self, error: io::Error) -> ConvertError {
        if error.kind() == io::ErrorKind::NotFound {
            ConvertError::AsciidoctorMissing {
                program: self.program.clone(),
            }
        } else {
            ConvertError::Io(error)
        }
    }

    fn command(&self, request: &ConvertRequest, overlay: Option<&Path>) -> Command {
        let base_dir = request.file.parent().unwrap_or(Path::new("."));
        let mut command = Command::new(&self.program);
        command
            .arg("-r")
            .arg(&self.extension.file)
            .args(["-s", "-a", "showtitle@", "-B"])
            .arg(base_dir)
            .arg("-a")
            .arg(format!("docfile={}", request.file.display()));
        // Stdin input leaves these unset, a file input derives them.
        if let Some(stem) = request.file.file_stem() {
            command
                .arg("-a")
                .arg(format!("docname={}", stem.to_string_lossy()));
        }
        if let Some(extension) = request.file.extension() {
            command
                .arg("-a")
                .arg(format!("docfilesuffix=.{}", extension.to_string_lossy()));
        }
        // The trailing `@` makes each a soft set, so the document header can
        // still override a config value; a plain `-a` locks the attribute.
        for (name, value) in &request.attributes {
            command.arg("-a").arg(format!("{name}={value}@"));
        }
        if let Some(overlay) = overlay {
            command.env(OVERLAY_ENV, overlay);
        }
        if let Some(root) = &self.root {
            command.env(ROOT_ENV, root);
        }
        command
            .args(["-o", "-", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }
}

impl Convert for AsciidoctorConverter {
    async fn convert(&self, request: &ConvertRequest) -> Result<String, ConvertError> {
        // Held until the run ends or its future is dropped.
        let overlay = if request.overlay.is_empty() {
            None
        } else {
            Some(overlay_file(&request.overlay)?)
        };
        let mut child = self
            .command(
                request,
                overlay.as_ref().map(|overlay| overlay.file.as_path()),
            )
            .spawn()
            .map_err(|error| self.spawn_error(error))?;
        let stdin = child.stdin.take();
        let text = request.text.as_bytes();
        let write = async {
            let Some(mut stdin) = stdin else {
                return Ok(());
            };
            match stdin.write_all(text).await {
                // The process may exit before it reads everything; its exit
                // status says why.
                Err(error) if error.kind() != io::ErrorKind::BrokenPipe => Err(error),
                _ => Ok(()),
            }
        };
        let (written, output) = tokio::join!(write, child.wait_with_output());
        let output = output?;
        if !output.status.success() {
            return Err(run_failed(&output));
        }
        written?;
        String::from_utf8(output.stdout).map_err(|_| ConvertError::InvalidOutput)
    }
}

/// The content of the first `<style>` element of a standalone page.
fn extract_stylesheet(page: &str) -> Option<String> {
    let start = page.find("<style>")? + "<style>".len();
    let length = page[start..].find("</style>")?;
    Some(page[start..start + length].trim().to_owned())
}

/// Whether a real Asciidoctor is installed, for tests that need one. A test
/// that finds none skips itself, unless `ASCIIDOC_LSP_REQUIRE_ASCIIDOCTOR` is
/// set: then the missing Asciidoctor fails the test, so CI cannot pass
/// without having run it.
#[cfg(test)]
pub(crate) fn asciidoctor_available() -> bool {
    let available = std::process::Command::new("asciidoctor")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !available {
        assert!(
            std::env::var_os("ASCIIDOC_LSP_REQUIRE_ASCIIDOCTOR").is_none(),
            "asciidoctor is not on the PATH, but ASCIIDOC_LSP_REQUIRE_ASCIIDOCTOR is set"
        );
        eprintln!("skipped: asciidoctor is not on the PATH");
    }
    available
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Converts to the request text after `delay`; counts finished and
    /// aborted runs.
    struct Fake {
        delay: Duration,
        finished: Arc<AtomicUsize>,
        started: Arc<AtomicUsize>,
    }

    impl Convert for Fake {
        async fn convert(&self, request: &ConvertRequest) -> Result<String, ConvertError> {
            self.started.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.finished.fetch_add(1, Ordering::SeqCst);
            Ok(request.text.clone())
        }
    }

    #[test]
    fn extracts_the_first_style_element() {
        let page = "<head><link rel=x>\n<style>\nbody { margin: 0 }\n</style>\n<style>b</style>";
        assert_eq!(
            extract_stylesheet(page).as_deref(),
            Some("body { margin: 0 }")
        );
        assert_eq!(extract_stylesheet("<p>no style</p>"), None);
    }

    fn request(text: &str) -> ConvertRequest {
        ConvertRequest {
            file: PathBuf::from("/doc.adoc"),
            text: text.to_owned(),
            attributes: Vec::new(),
            overlay: Vec::new(),
        }
    }

    fn fake(delay: Duration) -> (Fake, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let started = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));
        let fake = Fake {
            delay,
            finished: finished.clone(),
            started: started.clone(),
        };
        (fake, started, finished)
    }

    /// Lets the worker task run until it waits again; time stays put.
    async fn settle() {
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
    }

    async fn next_html(outcomes: &mut mpsc::UnboundedReceiver<ConvertOutcome>) -> String {
        let outcome = tokio::time::timeout(Duration::from_secs(5), outcomes.recv())
            .await
            .unwrap()
            .unwrap();
        outcome.result.unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn test_burst_within_debounce_converts_only_the_last() {
        let (fake, started, _) = fake(Duration::from_millis(1));
        let (handle, mut outcomes) = spawn(fake, Duration::from_millis(60));
        handle.request(request("a"));
        handle.request(request("b"));
        handle.request(request("c"));
        settle().await;
        tokio::time::advance(Duration::from_millis(59)).await;
        settle().await;
        assert_eq!(started.load(Ordering::SeqCst), 0);
        tokio::time::advance(Duration::from_millis(1)).await;
        settle().await;
        assert_eq!(started.load(Ordering::SeqCst), 1);
        assert_eq!(next_html(&mut outcomes).await, "c");
        assert_eq!(started.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn test_requests_spread_over_time_reset_the_debounce() {
        let (fake, started, _) = fake(Duration::from_millis(1));
        let (handle, mut outcomes) = spawn(fake, Duration::from_millis(150));
        // Each gap is shorter than the debounce, the whole burst is longer.
        for text in ["a", "b", "c", "d"] {
            handle.request(request(text));
            settle().await;
            tokio::time::advance(Duration::from_millis(80)).await;
            settle().await;
        }
        assert_eq!(started.load(Ordering::SeqCst), 0);
        // 80 ms after the last request: 70 ms are still missing.
        tokio::time::advance(Duration::from_millis(69)).await;
        settle().await;
        assert_eq!(started.load(Ordering::SeqCst), 0);
        tokio::time::advance(Duration::from_millis(1)).await;
        settle().await;
        assert_eq!(started.load(Ordering::SeqCst), 1);
        assert_eq!(next_html(&mut outcomes).await, "d");
        assert_eq!(started.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn test_outcome_generation_marks_stale_results() {
        let (fake, _, _) = fake(Duration::from_millis(1));
        let (handle, mut outcomes) = spawn(fake, Duration::from_millis(10));
        let latest = handle.latest_generation();
        handle.request(request("a"));
        let outcome = outcomes.recv().await.unwrap();
        assert_eq!(outcome.generation, 1);
        assert!(!outcome.is_stale(&latest));
        handle.request(request("b"));
        assert!(outcome.is_stale(&latest));
    }

    #[test]
    fn test_command_sets_config_attributes_softly_and_docname() {
        let converter = AsciidoctorConverter::with_program("asciidoctor").unwrap();
        let mut request = request("= T");
        request.file = PathBuf::from("/work/guide.adoc");
        request.attributes = vec![("toc".to_owned(), "left".to_owned())];
        let command = converter.command(&request, None);
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        for expected in [
            "toc=left@",
            "docname=guide",
            "docfilesuffix=.adoc",
            "docfile=/work/guide.adoc",
        ] {
            assert!(
                args.iter().any(|arg| arg == expected),
                "{expected}: {args:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_header_attribute_overrides_config_attribute() {
        if !asciidoctor_available() {
            return;
        }
        let converter = AsciidoctorConverter::new().unwrap();
        let mut request = request("= T\n:foo: header\n\n{foo} {docname}\n");
        request.file = PathBuf::from("/work/guide.adoc");
        request.attributes = vec![("foo".to_owned(), "config".to_owned())];
        let html = converter.convert(&request).await.unwrap();
        assert!(html.contains("header guide"), "{html}");
    }

    #[tokio::test(start_paused = true)]
    async fn test_newer_request_aborts_the_running_conversion() {
        let (fake, started, finished) = fake(Duration::from_millis(400));
        let (handle, mut outcomes) = spawn(fake, Duration::from_millis(10));
        handle.request(request("old"));
        settle().await;
        tokio::time::advance(Duration::from_millis(10)).await;
        settle().await;
        // The old run started; 100 ms into its 400 ms a newer request comes.
        assert_eq!(started.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_millis(100)).await;
        settle().await;
        handle.request(request("new"));
        assert_eq!(next_html(&mut outcomes).await, "new");
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert_eq!(finished.load(Ordering::SeqCst), 1);
        assert!(outcomes.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn test_worker_ends_when_handle_is_dropped() {
        let (fake, _, _) = fake(Duration::from_millis(1));
        let (handle, mut outcomes) = spawn(fake, Duration::from_millis(10));
        drop(handle);
        assert!(outcomes.recv().await.is_none());
    }

    #[tokio::test]
    async fn test_missing_asciidoctor_is_a_typed_error() {
        let converter = AsciidoctorConverter::with_program("asciidoctor-does-not-exist").unwrap();
        let error = converter.convert(&request("= T")).await.unwrap_err();
        assert!(matches!(error, ConvertError::AsciidoctorMissing { .. }));
        assert!(error.to_string().contains("install Asciidoctor"));
    }

    #[tokio::test]
    async fn test_failing_process_reports_status_and_stderr() {
        // `false` ignores its arguments and exits 1.
        let converter = AsciidoctorConverter::with_program("false").unwrap();
        let error = converter.convert(&request("= T")).await.unwrap_err();
        assert!(matches!(error, ConvertError::Failed { .. }));
    }

    #[tokio::test]
    async fn test_nested_include_html_carries_source_lines_and_file_table() {
        if !asciidoctor_available() {
            return;
        }
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/nested_include")
            .canonicalize()
            .unwrap();
        let file = dir.join("index.adoc");
        let converter = AsciidoctorConverter::new().unwrap();
        let html = converter
            .convert(&ConvertRequest {
                text: std::fs::read_to_string(&file).unwrap(),
                file,
                attributes: Vec::new(),
                overlay: Vec::new(),
            })
            .await
            .unwrap();
        // A block of the main file has a line but no `data-src`.
        assert!(html.contains(r#"class="paragraph data-line-5""#), "{html}");
        // A block of inner.adoc (file 1) after two nested includes: its own
        // line, not the line of the include that pulled it in.
        assert!(
            html.contains(r#"class="paragraph data-src-1 data-line-3""#),
            "{html}"
        );
        assert!(html.contains("data-src-0"), "{html}");
        assert!(html.contains(r#"id="adoc-source-files""#), "{html}");
        assert!(html.contains("mid.adoc"), "{html}");
    }

    #[tokio::test]
    async fn test_nested_include_html_lists_the_includes() {
        if !asciidoctor_available() {
            return;
        }
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/nested_include")
            .canonicalize()
            .unwrap();
        let file = dir.join("index.adoc");
        let html = AsciidoctorConverter::new()
            .unwrap()
            .convert(&ConvertRequest {
                text: std::fs::read_to_string(&file).unwrap(),
                file,
                attributes: Vec::new(),
                overlay: Vec::new(),
            })
            .await
            .unwrap();
        let script = |id: &str| {
            let open = format!(r#"<script type="application/json" id="{id}">"#);
            let start = html.find(&open).unwrap() + open.len();
            let end = start + html[start..].find("</script>").unwrap();
            serde_json::from_str::<serde_json::Value>(&html[start..end]).unwrap()
        };
        let files = script("adoc-source-files");
        assert_eq!(files.as_array().unwrap().len(), 2, "{html}");
        // mid.adoc is file 0, inner.adoc file 1. The main file is `null`;
        // the directives stand on line 3 of index.adoc and of mid.adoc.
        assert_eq!(
            script("adoc-source-includes"),
            serde_json::json!([
                {"parent": null, "line": 3, "file": 0},
                {"parent": 0, "line": 3, "file": 1},
            ]),
            "{html}"
        );
    }

    #[tokio::test]
    async fn test_include_table_covers_option_and_conditional_includes() {
        if !asciidoctor_available() {
            return;
        }
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/include_table")
            .canonicalize()
            .unwrap();
        let file = dir.join("index.adoc");
        let html = AsciidoctorConverter::new()
            .unwrap()
            .convert(&ConvertRequest {
                text: std::fs::read_to_string(&file).unwrap(),
                file,
                attributes: Vec::new(),
                overlay: Vec::new(),
            })
            .await
            .unwrap();
        let script = |id: &str| {
            let open = format!(r#"<script type="application/json" id="{id}">"#);
            let start = html.find(&open).unwrap() + open.len();
            let end = start + html[start..].find("</script>").unwrap();
            serde_json::from_str::<serde_json::Value>(&html[start..end]).unwrap()
        };
        // offset (leveloffset), tagged (tags=), lines (lines=) and guarded
        // (inside ifdef) are files 0 to 3; each directive is on its own line
        // of index.adoc, the guarded one below its `ifdef` line.
        assert_eq!(
            script("adoc-source-includes"),
            serde_json::json!([
                {"parent": null, "line": 4, "file": 0},
                {"parent": null, "line": 6, "file": 1},
                {"parent": null, "line": 8, "file": 2},
                {"parent": null, "line": 11, "file": 3},
            ]),
            "{html}"
        );
        // The blocks carry the lines of the selected part of their file.
        assert!(html.contains("sect1 data-src-0 data-line-1"), "{html}");
        assert!(html.contains("paragraph data-src-1 data-line-4"), "{html}");
        assert!(html.contains("paragraph data-src-2 data-line-3"), "{html}");
        assert!(html.contains("paragraph data-src-3 data-line-1"), "{html}");
    }

    /// Convert `text` as `/doc.adoc` without a workspace root.
    async fn convert_plain(text: &str) -> String {
        AsciidoctorConverter::new()
            .unwrap()
            .convert(&request(text))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_document_title_is_shown() {
        if !asciidoctor_available() {
            return;
        }
        let html = convert_plain("= The Title\n\n== Section\n\nText.\n").await;
        assert!(html.contains("<h1>The Title</h1>"), "{html}");
    }

    #[tokio::test]
    async fn test_document_can_hide_its_title() {
        if !asciidoctor_available() {
            return;
        }
        let html = convert_plain("= The Title\n:!showtitle:\n\nText.\n").await;
        assert!(!html.contains("<h1"), "{html}");
    }

    /// The `adoc-body-classes` table: the classes the page puts on its body.
    fn body_classes(html: &str) -> serde_json::Value {
        let open = r#"<script type="application/json" id="adoc-body-classes">"#;
        let start = html
            .find(open)
            .unwrap_or_else(|| panic!("no table: {html}"))
            + open.len();
        let end = start + html[start..].find("</script>").unwrap();
        serde_json::from_str(&html[start..end]).unwrap()
    }

    #[tokio::test]
    async fn test_toc_left_is_a_sidebar() {
        if !asciidoctor_available() {
            return;
        }
        let html = convert_plain("= T\n:toc: left\n\n== A\n\nText.\n").await;
        assert!(html.contains(r#"<div id="toc" class="toc2">"#), "{html}");
        assert_eq!(body_classes(&html), serde_json::json!(["toc2", "toc-left"]));
    }

    #[tokio::test]
    async fn test_toc_right_is_a_sidebar() {
        if !asciidoctor_available() {
            return;
        }
        let html = convert_plain("= T\n:toc: right\n\n== A\n\nText.\n").await;
        assert!(html.contains(r#"<div id="toc" class="toc2">"#), "{html}");
        assert_eq!(
            body_classes(&html),
            serde_json::json!(["toc2", "toc-right"])
        );
    }

    #[tokio::test]
    async fn test_toc_class_attribute_names_the_toc_class() {
        if !asciidoctor_available() {
            return;
        }
        let html = convert_plain("= T\n:toc: left\n:toc-class: side\n\n== A\n\nText.\n").await;
        assert!(html.contains(r#"<div id="toc" class="side">"#), "{html}");
        assert_eq!(body_classes(&html), serde_json::json!(["side", "toc-left"]));
    }

    #[tokio::test]
    async fn test_toc_class_with_several_classes_reaches_the_body() {
        if !asciidoctor_available() {
            return;
        }
        let html = convert_plain("= T\n:toc: left\n:toc-class: toc2 wide\n\n== A\n\nText.\n").await;
        assert!(
            html.contains(r#"<div id="toc" class="toc2 wide">"#),
            "{html}"
        );
        assert_eq!(
            body_classes(&html),
            serde_json::json!(["toc2", "wide", "toc-left"])
        );
    }

    #[tokio::test]
    async fn test_sidebar_toc_without_a_title_is_rewritten() {
        if !asciidoctor_available() {
            return;
        }
        let html = convert_plain("= T\n:!showtitle:\n:toc: left\n\n== A\n\nText.\n").await;
        assert!(html.starts_with(r#"<div id="toc" class="toc2">"#), "{html}");
        assert_eq!(body_classes(&html), serde_json::json!(["toc2", "toc-left"]));
    }

    #[tokio::test]
    async fn test_no_body_classes_when_the_toc_div_is_not_rewritten() {
        if !asciidoctor_available() {
            return;
        }
        // The manpage embedded output puts lines before the toc div.
        let html = convert_plain(
            "= t(1)\n:doctype: manpage\n:toc: left\n\n== NAME\n\nt - x\n\n== SYNOPSIS\n\nt\n",
        )
        .await;
        assert!(!html.contains(r#"class="toc2""#), "{html}");
        assert_eq!(body_classes(&html), serde_json::json!([]), "{html}");
    }

    #[tokio::test]
    async fn test_toc_auto_keeps_the_inline_toc() {
        if !asciidoctor_available() {
            return;
        }
        for header in [":toc:", ":toc: auto", ":toc: preamble", ":toc: macro"] {
            let html = convert_plain(&format!("= T\n{header}\n\n== A\n\nText.\n")).await;
            assert!(!html.contains("toc2"), "{header}: {html}");
            assert_eq!(body_classes(&html), serde_json::json!([]), "{header}");
        }
        let html = convert_plain("= T\n:toc:\n\n== A\n\nText.\n").await;
        assert!(html.contains(r#"<div id="toc" class="toc">"#), "{html}");
    }

    /// Convert `text` as `file` with `root` as the workspace root.
    async fn convert_with_root(root: &Path, file: PathBuf, text: &str) -> String {
        AsciidoctorConverter::new()
            .unwrap()
            .with_root(root.to_owned())
            .convert(&ConvertRequest {
                file,
                text: text.to_owned(),
                attributes: Vec::new(),
                overlay: Vec::new(),
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_local_images_point_at_the_files_route() {
        if !asciidoctor_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("chapters")).unwrap();
        let chapter = root.join("chapters/ch1.adoc");
        let text = "image::../images/block.png[Block]\n\n\
                    image::../images/my pic.png[Spaced]\n\n\
                    Inline image:../images/inline.png[Inline] here.\n\n\
                    image::https://example.org/remote.png[Remote]\n\n\
                    image::data:image/png;base64,AAAA[Data]\n\n\
                    image:://cdn.example.org/proto.png[Proto]\n\n\
                    image::../../outside.png[Outside]\n";
        let html = convert_with_root(&root, chapter, text).await;
        assert!(html.contains(r#"src="files/images/block.png""#), "{html}");
        assert!(html.contains(r#"src="files/images/inline.png""#), "{html}");
        assert!(
            html.contains(r#"src="files/images/my%20pic.png""#),
            "{html}"
        );
        assert!(
            html.contains(r#"src="https://example.org/remote.png""#),
            "{html}"
        );
        assert!(
            html.contains(r#"src="data:image/png;base64,AAAA""#),
            "{html}"
        );
        assert!(
            html.contains(r#"src="//cdn.example.org/proto.png""#),
            "{html}"
        );
        // Outside the root: left alone, it will not be found.
        assert!(html.contains(r#"src="../../outside.png""#), "{html}");
        // The list at the end names the rewritten images, decoded, and no
        // others.
        assert!(
            html.ends_with(
                "id=\"adoc-image-files\">[\"images/block.png\",\"images/my pic.png\",\"images/inline.png\"]</script>\n"
            ),
            "{html}"
        );
    }

    #[tokio::test]
    async fn test_images_follow_imagesdir_relative_and_absolute() {
        if !asciidoctor_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let book = root.join("book/book.adoc");
        std::fs::create_dir_all(root.join("book")).unwrap();
        let relative = ":imagesdir: ../pics\n\nimage::a.png[]\n";
        let html = convert_with_root(&root, book.clone(), relative).await;
        assert!(html.contains(r#"src="files/pics/a.png""#), "{html}");
        let absolute = format!(":imagesdir: {}/abs\n\nimage::b.png[]\n", root.display());
        let html = convert_with_root(&root, book, &absolute).await;
        assert!(html.contains(r#"src="files/abs/b.png""#), "{html}");
    }

    #[tokio::test]
    async fn test_tilde_image_targets_are_plain_relative_paths() {
        if !asciidoctor_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let text = "image::~draft.png[One]\n\nimage::~/home.png[Two]\n\nimage::~nosuchuser_x/a.png[Three]\n";
        let html = convert_with_root(&root, root.join("doc.adoc"), text).await;
        assert!(html.contains(r#"src="files/~draft.png""#), "{html}");
        assert!(html.contains(r#"src="files/~/home.png""#), "{html}");
        assert!(
            html.contains(r#"src="files/~nosuchuser_x/a.png""#),
            "{html}"
        );
    }

    #[tokio::test]
    async fn test_non_ascii_text_with_percent_escapes_does_not_abort_the_conversion() {
        if !asciidoctor_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let text = "image::Bild-\u{e4}%C3%A4.png[Mixed]\n\nimage::ok.png[Ok]\n";
        let html = convert_with_root(&root, root.join("doc.adoc"), text).await;
        assert!(html.contains(r#"src="files/ok.png""#), "{html}");
        assert!(html.contains("Mixed"), "{html}");
    }

    #[tokio::test]
    async fn test_a_drive_letter_is_not_a_url_scheme() {
        if !asciidoctor_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        // On Linux `C:/x` is a relative folder below the base directory, so a
        // rewrite shows that the drive letter was taken as a path.
        let text = ":imagesdir: C:/x\n\nimage::img.png[Drive]\n";
        let html = convert_with_root(&root, root.join("doc.adoc"), text).await;
        assert!(html.contains(r#"src="files/C%3A/x/img.png""#), "{html}");
        // A two-letter scheme still is a URL.
        let html =
            convert_with_root(&root, root.join("doc.adoc"), "image::ab://x/y.png[Url]\n").await;
        assert!(html.contains(r#"src="ab://x/y.png""#), "{html}");
    }

    #[tokio::test]
    async fn test_without_a_root_image_urls_stay_as_they_are() {
        if !asciidoctor_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("doc.adoc");
        let html = AsciidoctorConverter::new()
            .unwrap()
            .convert(&ConvertRequest {
                file,
                text: "image::images/a.png[]\n".to_owned(),
                attributes: Vec::new(),
                overlay: Vec::new(),
            })
            .await
            .unwrap();
        assert!(html.contains(r#"src="images/a.png""#), "{html}");
    }

    /// A chapter tree where the overlay replaces what is on disk.
    fn write_book(dir: &Path) {
        std::fs::create_dir_all(dir.join("chapters/sub")).unwrap();
        let files = [
            (
                "book.adoc",
                "= Book\n\ninclude::chapters/one.adoc[leveloffset=+1]\n\n\
                 include::chapters/two.adoc[tag=keep]\n\n\
                 include::chapters/three.adoc[lines=2..3]\n",
            ),
            (
                "chapters/one.adoc",
                "= One\n\ndisk text\n\ninclude::sub/inner.adoc[]\n",
            ),
            ("chapters/sub/inner.adoc", "disk inner\n"),
            (
                "chapters/two.adoc",
                "// tag::keep[]\ndisk kept\n// end::keep[]\n\ndisk dropped\n",
            ),
            ("chapters/three.adoc", "disk l1\n\ndisk l3\n\ndisk l5\n"),
        ];
        for (name, text) in files {
            std::fs::write(dir.join(name), text).unwrap();
        }
    }

    fn overlay_of(dir: &Path, entries: &[(&str, &str)]) -> Vec<(PathBuf, String)> {
        entries
            .iter()
            .map(|(name, text)| (dir.join(name), (*text).to_owned()))
            .collect()
    }

    #[tokio::test]
    async fn test_overlay_replaces_included_files_and_keeps_include_semantics() {
        if !asciidoctor_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        write_book(&dir);
        let file = dir.join("book.adoc");
        let overlay = overlay_of(
            &dir,
            &[
                // The nested include is relative to this file's folder.
                (
                    "chapters/one.adoc",
                    "= One\n\nunsaved text\n\ninclude::sub/inner.adoc[]\n",
                ),
                ("chapters/sub/inner.adoc", "unsaved inner\n"),
                (
                    "chapters/two.adoc",
                    "// tag::keep[]\nunsaved kept\n// end::keep[]\n\nunsaved dropped\n",
                ),
                (
                    "chapters/three.adoc",
                    "unsaved l1\n\nunsaved l3\n\nunsaved l5\n",
                ),
            ],
        );
        let converter = AsciidoctorConverter::new().unwrap();
        let html = converter
            .convert(&ConvertRequest {
                text: std::fs::read_to_string(&file).unwrap(),
                file,
                attributes: Vec::new(),
                overlay,
            })
            .await
            .unwrap();
        assert!(!html.contains("disk"), "{html}");
        assert!(html.contains("unsaved text"), "{html}");
        assert!(html.contains("unsaved inner"), "{html}");
        // leveloffset=+1 turns the chapter title into a section title.
        assert!(html.contains("<h2"), "{html}");
        assert!(html.contains("unsaved kept"), "{html}");
        assert!(!html.contains("unsaved dropped"), "{html}");
        // lines=2..3 selects the blank line and the third line.
        assert!(html.contains("unsaved l3"), "{html}");
        assert!(!html.contains("unsaved l1"), "{html}");
        assert!(!html.contains("unsaved l5"), "{html}");
    }

    #[tokio::test]
    async fn test_overlay_file_that_is_not_on_disk_can_be_included() {
        if !asciidoctor_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let converter = AsciidoctorConverter::new().unwrap();
        let html = converter
            .convert(&ConvertRequest {
                file: dir.join("book.adoc"),
                text: "= Book\n\ninclude::fresh.adoc[]\n".to_owned(),
                attributes: Vec::new(),
                overlay: overlay_of(&dir, &[("fresh.adoc", "only in the buffer\n")]),
            })
            .await
            .unwrap();
        assert!(html.contains("only in the buffer"), "{html}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_overlay_matches_an_include_through_a_symlinked_folder() {
        if !asciidoctor_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        std::fs::create_dir(dir.join("real")).unwrap();
        std::fs::write(dir.join("real/ch.adoc"), "disk text\n").unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("shared")).unwrap();
        let converter = AsciidoctorConverter::new().unwrap();
        // The LSP keys the overlay by the canonical path.
        let html = converter
            .convert(&ConvertRequest {
                file: dir.join("book.adoc"),
                text: "= Book\n\ninclude::shared/ch.adoc[]\n".to_owned(),
                attributes: Vec::new(),
                overlay: overlay_of(&dir, &[("real/ch.adoc", "unsaved text\n")]),
            })
            .await
            .unwrap();
        assert!(html.contains("unsaved text"), "{html}");
        // The file table names the canonical path too.
        assert!(
            html.contains(&dir.join("real/ch.adoc").display().to_string()),
            "{html}"
        );
    }

    #[tokio::test]
    async fn test_without_overlay_includes_come_from_disk() {
        if !asciidoctor_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        write_book(&dir);
        let file = dir.join("book.adoc");
        let converter = AsciidoctorConverter::new().unwrap();
        let html = converter
            .convert(&ConvertRequest {
                text: std::fs::read_to_string(&file).unwrap(),
                file,
                attributes: Vec::new(),
                overlay: Vec::new(),
            })
            .await
            .unwrap();
        assert!(html.contains("disk text"), "{html}");
        assert!(html.contains("disk inner"), "{html}");
    }

    #[cfg(unix)]
    #[test]
    fn test_overlay_file_is_private_and_removed_on_drop() {
        use std::os::unix::fs::PermissionsExt;
        let overlay = overlay_file(&[(PathBuf::from("/a.adoc"), "x".to_owned())]).unwrap();
        let dir = overlay.dir.clone();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&overlay.file), 0o600);
        let json = std::fs::read_to_string(&overlay.file).unwrap();
        assert_eq!(json, r#"{"/a.adoc":"x"}"#);
        drop(overlay);
        assert!(!dir.exists());
    }
}
