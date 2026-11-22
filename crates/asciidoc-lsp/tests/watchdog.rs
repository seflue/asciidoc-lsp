//! The server ends itself when the client process named in `initialize`
//! dies, even while another process keeps its stdin open.
#![cfg(unix)]

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

fn send(stdin: &mut impl Write, body: &str) {
    write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    stdin.flush().unwrap();
}

fn exited_within(server: &mut Child, bound: Duration) -> bool {
    let deadline = Instant::now() + bound;
    while Instant::now() < deadline {
        if server.try_wait().unwrap().is_some() {
            return true;
        }
        sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn server_exits_when_client_process_dies_while_pipe_stays_open() {
    let mut client = Command::new("sleep").arg("60").spawn().unwrap();
    let mut server = Command::new(env!("CARGO_BIN_EXE_asciidoc-lsp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Held until the end of the test: the pipe never closes.
    let mut stdin = server.stdin.take().unwrap();
    let _stdout = server.stdout.take().unwrap();
    send(
        &mut stdin,
        &format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"processId":{},"capabilities":{{}}}}}}"#,
            client.id()
        ),
    );

    let still_running = !exited_within(&mut server, Duration::from_millis(2500));

    client.kill().unwrap();
    client.wait().unwrap();
    let exited = exited_within(&mut server, Duration::from_secs(10));
    if !exited {
        server.kill().unwrap();
    }
    let status = server.wait().unwrap();

    assert!(still_running, "server ended while the client process lived");
    assert!(
        exited,
        "server still running 10 s after the client process died"
    );
    assert!(status.success(), "server exited with {status}, expected 0");
}
