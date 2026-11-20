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

/// The extension file for `-r`, in a private temporary folder that is
/// removed on drop.
#[derive(Debug)]
struct ExtensionFile {
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

impl ExtensionFile {
    fn write() -> io::Result<Self> {
        let dir = private_dir()?;
        let file = dir.join("sourceline.rb");
        if let Err(error) = write_private(&file, EXTENSION) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(error);
        }
        Ok(Self { dir, file })
    }
}

impl Drop for ExtensionFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The overlay of one run as a JSON file `{absolute path: text}` in a private
/// folder, removed on drop. The extension reads it from the environment
/// variable [`OVERLAY_ENV`], so no path or text reaches the command line or
/// the document's attributes.
#[derive(Debug)]
struct OverlayFile {
    dir: PathBuf,
    file: PathBuf,
}

impl OverlayFile {
    fn write(entries: &[(PathBuf, String)]) -> io::Result<Self> {
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
        let dir = private_dir()?;
        let file = dir.join("overlay.json");
        if let Err(error) = write_private(&file, &json) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(error);
        }
        Ok(Self { dir, file })
    }
}

impl Drop for OverlayFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Converts with the `asciidoctor` executable.
#[derive(Debug)]
pub struct AsciidoctorConverter {
    program: String,
    extension: ExtensionFile,
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
            extension: ExtensionFile::write()?,
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
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    ConvertError::AsciidoctorMissing {
                        program: self.program.clone(),
                    }
                } else {
                    ConvertError::Io(error)
                }
            })?;
        if !output.status.success() {
            return Err(ConvertError::Failed {
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        let page = String::from_utf8(output.stdout).map_err(|_| ConvertError::InvalidOutput)?;
        extract_stylesheet(&page).ok_or(ConvertError::StylesheetMissing)
    }

    fn command(&self, request: &ConvertRequest, overlay: Option<&Path>) -> Command {
        let base_dir = request.file.parent().unwrap_or(Path::new("."));
        let mut command = Command::new(&self.program);
        command
            .arg("-r")
            .arg(&self.extension.file)
            .args(["-s", "-B"])
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
            Some(OverlayFile::write(&request.overlay)?)
        };
        let mut child = self
            .command(
                request,
                overlay.as_ref().map(|overlay| overlay.file.as_path()),
            )
            .spawn()
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    ConvertError::AsciidoctorMissing {
                        program: self.program.clone(),
                    }
                } else {
                    ConvertError::Io(error)
                }
            })?;
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
            return Err(ConvertError::Failed {
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
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
            eprintln!("skipped: asciidoctor is not on the PATH");
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

    fn asciidoctor_available() -> bool {
        std::process::Command::new("asciidoctor")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    #[tokio::test]
    async fn test_nested_include_html_carries_source_lines_and_file_table() {
        if !asciidoctor_available() {
            eprintln!("skipped: asciidoctor is not on the PATH");
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
            eprintln!("skipped: asciidoctor is not on the PATH");
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
    }

    #[tokio::test]
    async fn test_images_follow_imagesdir_relative_and_absolute() {
        if !asciidoctor_available() {
            eprintln!("skipped: asciidoctor is not on the PATH");
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
    async fn test_without_a_root_image_urls_stay_as_they_are() {
        if !asciidoctor_available() {
            eprintln!("skipped: asciidoctor is not on the PATH");
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
            eprintln!("skipped: asciidoctor is not on the PATH");
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
            eprintln!("skipped: asciidoctor is not on the PATH");
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
            eprintln!("skipped: asciidoctor is not on the PATH");
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
            eprintln!("skipped: asciidoctor is not on the PATH");
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
        let overlay = OverlayFile::write(&[(PathBuf::from("/a.adoc"), "x".to_owned())]).unwrap();
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
