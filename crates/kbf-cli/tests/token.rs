//! `kbf token` end to end: the real binary against a one-shot HTTP server that keeps the
//! request it was sent. The verbs' requests and the `--out` file are pinned in
//! `src/token/tests.rs`; this pins that the binary dispatches the word, and that the
//! secret never reaches its stdout when `--out` is given. `kbf-server`'s
//! `tests/enroll_api.rs` runs the same client against the real routes.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_kbf");
const HANG: Duration = Duration::from_secs(20);

const CREATED: &str = r#"{"token":"kbft1.aaaaaaaaaaaa.SECRETPART.PINPART","secret":"SECRETPART","info":{"id":"aaaaaaaaaaaa","roles":["daemon"],"max_uses":1,"uses":0,"expires_at":100,"created_at":10,"revoked_at":null,"node_id":null,"node_prefix":null,"names":[],"replace":false,"label":"","created_by":"operator-api"}}"#;

/// Answers one request with 200 and `body`; the thread returns the request as sent.
fn serve_once(body: &'static str) -> (SocketAddr, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("non-blocking");
    let addr = listener.local_addr().expect("addr");
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + HANG;
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("kbf never called the server: {e}"),
            }
        };
        stream.set_nonblocking(false).expect("blocking");
        stream.set_read_timeout(Some(HANG)).expect("timeout");
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = stream.read(&mut buf).expect("read the request");
            request.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&request);
            if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                let length = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().expect("a length"))
                    })
                    .unwrap_or(0);
                if rest.len() >= length {
                    break;
                }
            }
            assert!(n > 0, "the request ended early");
        }
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(reply.as_bytes()).expect("reply");
        String::from_utf8(request).expect("UTF-8")
    });
    (addr, thread)
}

fn scratch() -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("kbf-token-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a directory");
    dir
}

// Catches `kbf token` not being dispatched (it would be read as a mode), a create that
// prints the token string when it was told to write it to a file, a token file with the
// umask's mode, and a help that does not tell an operator the word exists.
#[test]
fn the_binary_writes_the_token_to_a_file_and_prints_none_of_it() {
    let dir = scratch();
    let api_token = dir.join("api-token");
    std::fs::write(&api_token, "kbf-test-token-0123456789abcdef0123456789\n").expect("write");
    let out = dir.join("enroll-token");
    let (addr, request) = serve_once(CREATED);
    let result = Command::new(BIN)
        .args(["token", "create", "--roles", "daemon", "--api"])
        .arg(addr.to_string())
        .arg("--token-file")
        .arg(&api_token)
        .arg("--out")
        .arg(&out)
        .output()
        .expect("run kbf");
    let request = request.join().expect("the server thread");
    assert!(
        request.starts_with("POST /v1/enroll/tokens HTTP/1.1\r\n"),
        "{request}"
    );
    assert_eq!(result.status.code(), Some(0), "{result:?}");
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        !stdout.contains("SECRETPART") && !stdout.contains("kbft1"),
        "{stdout}"
    );
    assert!(stdout.contains("aaaaaaaaaaaa"), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(&out).expect("the file"),
        "kbft1.aaaaaaaaaaaa.SECRETPART.PINPART\n"
    );
    let mode = std::fs::metadata(&out).expect("meta").permissions().mode();
    assert_eq!(mode & 0o7777, 0o600);

    let help = Command::new(BIN).arg("--help").output().expect("run kbf");
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("create|list|revoke"), "{help}");
    let bad = Command::new(BIN)
        .args(["token", "list"])
        .output()
        .expect("run kbf");
    assert_eq!(
        bad.status.code(),
        Some(2),
        "a missing --token-file is a usage error"
    );
}
