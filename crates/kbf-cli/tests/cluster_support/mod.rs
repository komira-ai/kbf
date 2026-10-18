//! The helpers of `tests/cluster.rs` and `tests/cluster_object_store.rs`: a one-shot HTTP
//! server that keeps the request it was sent, and `kbf cluster` run against it.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::process::Command;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_kbf");

/// How long a test waits for the binary to call the server: a hang deadline.
pub const HANG: Duration = Duration::from_secs(20);

/// Answers one request with `status` and `body`; the thread returns the request as it
/// was sent (head and body).
pub fn serve_once(status: &'static str, body: &'static str) -> (SocketAddr, JoinHandle<String>) {
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
        let wanted = loop {
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
                    break request.len();
                }
            }
            assert!(n > 0, "the request ended early");
        };
        assert_eq!(wanted, request.len());
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(reply.as_bytes()).expect("reply");
        String::from_utf8(request).expect("UTF-8")
    });
    (addr, thread)
}

pub fn kbf(args: &[&str]) -> (Option<i32>, String, String) {
    kbf_stdin(args, "")
}

/// `kbf args` with `stdin` as its standard input (closed after it).
pub fn kbf_stdin(args: &[&str], stdin: &str) -> (Option<i32>, String, String) {
    use std::process::Stdio;
    let mut child = Command::new(BIN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run kbf");
    let mut pipe = child.stdin.take().expect("a stdin pipe");
    // The binary may exit without reading it (a refusal), which breaks the pipe.
    let _ = pipe.write_all(stdin.as_bytes());
    drop(pipe);
    let out = child.wait_with_output().expect("kbf's exit");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A token file holding `token` (the client does not check its mode).
pub fn token_file(name: &str, token: &str) -> String {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("kbf-cluster");
    std::fs::create_dir_all(&dir).expect("a directory");
    let path = dir.join(format!("{name}-{}", std::process::id()));
    std::fs::write(&path, format!("{token}\n")).expect("write");
    path.display().to_string()
}

/// Runs `kbf cluster --api <server> <args>` against a server answering `status` and
/// `body`; the exit status, stdout, stderr and the request the server saw.
pub fn cluster(
    status: &'static str,
    body: &'static str,
    args: &[&str],
) -> (Option<i32>, String, String, String) {
    cluster_stdin(status, body, args, "")
}

/// [`cluster`] with `stdin` as the binary's standard input.
pub fn cluster_stdin(
    status: &'static str,
    body: &'static str,
    args: &[&str],
    stdin: &str,
) -> (Option<i32>, String, String, String) {
    let (addr, request) = serve_once(status, body);
    let api = addr.to_string();
    let mut argv = vec!["cluster", "--api", &api];
    argv.extend_from_slice(args);
    let (code, out, err) = kbf_stdin(&argv, stdin);
    (code, out, err, request.join().expect("the server thread"))
}
