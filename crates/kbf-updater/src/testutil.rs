//! Helpers shared by the tests of the listener, the adapter and the wiring.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::daemon::Config;
use crate::listener::Listen;
use crate::server::AuditSink;

pub(crate) const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample";

/// 36 visible characters, a token the file rule accepts.
pub(crate) const TOKEN: &str = "tok-0123456789abcdef0123456789abcdef";

pub(crate) fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn own_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// A fresh directory (mode 0755) under `target/`, short enough for a socket path.
pub(crate) fn scratch(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .join("target/updater-scratch")
        .join(format!(
            "{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
    std::fs::create_dir_all(&dir).expect("scratch");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    dir
}

/// Write `text` to `dir/name` with `mode`.
pub(crate) fn file(dir: &Path, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

/// The audit lines a server wrote.
#[derive(Clone, Default)]
pub(crate) struct Lines(Arc<Mutex<Vec<String>>>);

impl Lines {
    pub fn sink(&self) -> AuditSink {
        let list = Arc::clone(&self.0);
        Arc::new(move |line| locked(&list).push(line.to_owned()))
    }

    pub fn all(&self) -> Vec<serde_json::Value> {
        locked(&self.0)
            .iter()
            .map(|l| serde_json::from_str(l).expect("an audit line is JSON"))
            .collect()
    }

    pub fn raw(&self) -> String {
        locked(&self.0).join("\n")
    }

    /// Wait for the lines whose `event` is `event` to number `n`, and return them.
    pub fn events(&self, event: &str, n: usize) -> Vec<serde_json::Value> {
        eventually(|| self.of(event).len() >= n);
        self.of(event)
    }

    fn of(&self, event: &str) -> Vec<serde_json::Value> {
        self.all()
            .into_iter()
            .filter(|l| l["event"] == event)
            .collect()
    }
}

/// Wait up to 20 s for `cond`.
pub(crate) fn eventually(mut cond: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(Instant::now() < end, "the condition never came true");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A request as text.
pub(crate) fn raw(method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> String {
    let mut s = format!("{method} {path} HTTP/1.1\r\nHost: updater\r\n");
    for (k, v) in headers {
        s += &format!("{k}: {v}\r\n");
    }
    if !body.is_empty() || method == "POST" {
        s += &format!("Content-Length: {}\r\n", body.len());
    }
    s + "\r\n" + body
}

/// Send `bytes` to the socket and read until the server closes. Errors end the read: a server
/// that refuses a peer closes at once, and the write or the read may see the reset.
pub(crate) fn exchange(socket: &Path, bytes: &[u8]) -> Vec<u8> {
    let mut s = UnixStream::connect(socket).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(20)))
        .expect("timeout");
    let _ = s.write_all(bytes);
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    out
}

/// The status and JSON body of an answer (`Null` for no body, or none at all: status 0).
pub(crate) fn parsed(answer: &[u8]) -> (u16, serde_json::Value) {
    let text = String::from_utf8_lossy(answer);
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return (0, serde_json::Value::Null);
    };
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("a status");
    (
        status,
        serde_json::from_str(body).unwrap_or(serde_json::Value::Null),
    )
}

pub(crate) fn call(
    socket: &Path,
    method: &str,
    path: &str,
    body: &str,
) -> (u16, serde_json::Value) {
    parsed(&exchange(socket, raw(method, path, &[], body).as_bytes()))
}

/// An HTTP/1.1 answer.
pub(crate) fn http(status: u16, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A fake operator API on loopback. Each connection gets the next scripted answer (the last
/// one for ever after); `None` reads the request and then never answers.
pub(crate) struct FakeApi {
    pub addr: SocketAddr,
    pub seen: Arc<Mutex<Vec<String>>>,
}

pub(crate) fn fake_api(script: Vec<Option<Vec<u8>>>) -> FakeApi {
    fake_api_by(move |n, _| script[n.min(script.len() - 1)].clone())
}

/// The path a config binds.
pub(crate) fn sock(cfg: &Config) -> PathBuf {
    match &cfg.listen {
        Listen::Bind(path) => path.clone(),
        Listen::Activated(fd) => panic!("descriptor {fd} is activated, not bound"),
    }
}

/// A fake operator API that answers connection `n`, having read its request head, with what
/// `answer(n, head)` returns (`None`: never answer).
pub(crate) fn fake_api_by(
    answer: impl Fn(usize, &str) -> Option<Vec<u8>> + Send + 'static,
) -> FakeApi {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for (n, stream) in listener.incoming().flatten().enumerate() {
            let mut stream = stream;
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).is_ok_and(|n| n == 1) {
                head.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&head).into_owned();
            let reply = answer(n, &head);
            locked(&log).push(head);
            match reply {
                Some(answer) => {
                    let _ = stream.write_all(&answer);
                }
                None => held.push(stream),
            }
        }
    });
    FakeApi { addr, seen }
}

impl FakeApi {
    pub fn requests(&self) -> Vec<String> {
        locked(&self.seen).clone()
    }
}

/// An address nothing listens at.
pub(crate) fn closed_port() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("addr")
}
