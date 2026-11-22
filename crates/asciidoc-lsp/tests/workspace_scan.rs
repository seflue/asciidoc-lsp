//! The workspace scan runs in the background: `initialize` answers at once,
//! and `exit` ends the process while the scan of a large root is still
//! running.
#![cfg(unix)]

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Bound for things that must happen at once; only a hang reaches it.
const HANG_GUARD: Duration = Duration::from_secs(10);

/// A root whose scan never finishes: the scan opens every `*.adoc` file
/// for reading, and opening a FIFO without a writer blocks for good. It
/// stands in for a root so large that its scan outlasts the test, without
/// depending on how fast the machine scans. It relies on the scan not
/// filtering out non-regular files: if it ever does, this root stops
/// blocking and the test passes without proving anything.
fn blocking_root(dir: &std::path::Path) {
    let status = Command::new("mkfifo")
        .arg(dir.join("never-written.adoc"))
        .status()
        .unwrap();
    assert!(status.success(), "mkfifo failed: {status}");
}

fn frame(body: &str) -> Vec<u8> {
    format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
}

fn exited_within(server: &mut Child, bound: Duration) -> bool {
    let deadline = Instant::now() + bound;
    while Instant::now() < deadline {
        if server.try_wait().unwrap().is_some() {
            return true;
        }
        sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn exit_during_the_scan_ends_the_server_before_the_scan_finishes() {
    let tmp = tempfile::tempdir().unwrap();
    blocking_root(tmp.path());
    let root_uri = format!("file://{}", tmp.path().display());

    let mut child = Command::new(env!("CARGO_BIN_EXE_asciidoc-lsp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    // Everything the server writes, chunk by chunk, until EOF.
    let (chunks_tx, chunks) = mpsc::channel::<Vec<u8>>();
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n @ 1..) = stdout.read(&mut buf) {
            if chunks_tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let initialize = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"capabilities":{{}},"rootUri":"{root_uri}"}}}}"#
    );
    stdin.write_all(&frame(&initialize)).unwrap();
    stdin.flush().unwrap();

    let mut output = String::new();
    let deadline = Instant::now() + HANG_GUARD;
    let mut answered = true;
    while !output.contains(r#""id":1"#) {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(chunk) = chunks.recv_timeout(left) else {
            answered = false;
            break;
        };
        output.push_str(&String::from_utf8_lossy(&chunk));
    }

    let mut exited = false;
    if answered {
        stdin
            .write_all(&frame(
                r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
            ))
            .unwrap();
        stdin
            .write_all(&frame(r#"{"jsonrpc":"2.0","method":"exit"}"#))
            .unwrap();
        stdin.flush().unwrap();
        exited = exited_within(&mut child, HANG_GUARD);
    }
    // A hung server is killed here, so it never outlives the test; its
    // stdout closes and the reader thread ends.
    if !exited {
        child.kill().unwrap();
    }
    child.wait().unwrap();
    reader.join().unwrap();
    drop(stdin);

    assert!(
        answered,
        "initialize got no answer within {HANG_GUARD:?} while the scan ran"
    );
    assert!(
        exited,
        "server still running {HANG_GUARD:?} after exit while the scan ran"
    );
}
