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

impl ExtensionFile {
    fn write() -> io::Result<Self> {
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
                Ok(()) => {
                    let file = dir.join("sourceline.rb");
                    if let Err(error) = std::fs::write(&file, EXTENSION) {
                        let _ = std::fs::remove_dir_all(&dir);
                        return Err(error);
                    }
                    return Ok(Self { dir, file });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => last_error = error,
                Err(error) => return Err(error),
            }
        }
        Err(last_error)
    }
}

impl Drop for ExtensionFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Converts with the `asciidoctor` executable.
#[derive(Debug)]
pub struct AsciidoctorConverter {
    program: String,
    extension: ExtensionFile,
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
        })
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

    fn command(&self, request: &ConvertRequest) -> Command {
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
        let mut child = self.command(request).spawn().map_err(|error| {
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

    async fn next_html(outcomes: &mut mpsc::UnboundedReceiver<ConvertOutcome>) -> String {
        let outcome = tokio::time::timeout(Duration::from_secs(5), outcomes.recv())
            .await
            .unwrap()
            .unwrap();
        outcome.result.unwrap()
    }

    #[tokio::test]
    async fn test_burst_within_debounce_converts_only_the_last() {
        let (fake, started, _) = fake(Duration::from_millis(1));
        let (handle, mut outcomes) = spawn(fake, Duration::from_millis(60));
        handle.request(request("a"));
        handle.request(request("b"));
        handle.request(request("c"));
        assert_eq!(next_html(&mut outcomes).await, "c");
        assert_eq!(started.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_requests_spread_over_time_reset_the_debounce() {
        let (fake, started, _) = fake(Duration::from_millis(1));
        let (handle, mut outcomes) = spawn(fake, Duration::from_millis(150));
        // Each gap is shorter than the debounce, the whole burst is longer.
        for text in ["a", "b", "c", "d"] {
            handle.request(request(text));
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        assert_eq!(started.load(Ordering::SeqCst), 0);
        assert_eq!(next_html(&mut outcomes).await, "d");
        assert_eq!(started.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
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
        let command = converter.command(&request);
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

    #[tokio::test]
    async fn test_newer_request_aborts_the_running_conversion() {
        let (fake, started, finished) = fake(Duration::from_millis(400));
        let (handle, mut outcomes) = spawn(fake, Duration::from_millis(10));
        handle.request(request("old"));
        while started.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        handle.request(request("new"));
        assert_eq!(next_html(&mut outcomes).await, "new");
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert_eq!(finished.load(Ordering::SeqCst), 1);
        assert!(outcomes.try_recv().is_err());
    }

    #[tokio::test]
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
}
