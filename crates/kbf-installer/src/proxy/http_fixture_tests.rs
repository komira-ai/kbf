//! A fixture HTTP server on `127.0.0.1` for the tests of what calls a child's ports over
//! [`child_http`](super::child_http): [`Server`] answers every connection with a closure
//! that sees the request ([`Seen`]) and returns a [`Reply`], which may be delayed, and logs
//! the requests so a test sees what the client sent.
//!
//! It was the top of `admin_tests.rs` and is a file of its own so that the tests of the
//! fleet read use it too. It holds no test and ships nothing; it keeps the `_tests` name of
//! the file it came out of, which is how coverage tells test code from the code it measures.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::Value;

pub(super) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One request as the fixture read it.
#[derive(Clone, Debug)]
pub(super) struct Seen {
    pub(super) method: String,
    pub(super) path: String,
    /// Names in lower case.
    pub(super) headers: BTreeMap<String, String>,
    pub(super) body: Vec<u8>,
}

impl Seen {
    pub(super) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

/// What the fixture answers, after `delay`.
#[derive(Clone)]
pub(super) struct Reply {
    pub(super) status: u16,
    pub(super) body: String,
    pub(super) delay: Duration,
}

impl Reply {
    pub(super) fn json(status: u16, body: Value) -> Reply {
        Reply::text(status, &body.to_string())
    }

    pub(super) fn text(status: u16, body: &str) -> Reply {
        Reply {
            status,
            body: body.to_owned(),
            delay: Duration::ZERO,
        }
    }
}

type Handler = Box<dyn FnMut(&Seen) -> Reply + Send>;

/// A listener that answers every connection with `handler` and logs the requests.
pub(super) struct Server {
    pub(super) addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    pub(super) fn start(handler: impl FnMut(&Seen) -> Reply + Send + 'static) -> Server {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handler: Arc<Mutex<Handler>> = Arc::new(Mutex::new(Box::new(handler)));
        let thread = {
            let (seen, stop) = (Arc::clone(&seen), Arc::clone(&stop));
            thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let (seen, handler) = (Arc::clone(&seen), Arc::clone(&handler));
                    let _ =
                        stream.map(|stream| thread::spawn(move || serve(stream, &seen, &handler)));
                }
            })
        };
        Server {
            addr,
            seen,
            stop,
            thread: Some(thread),
        }
    }

    pub(super) fn seen(&self) -> Vec<Seen> {
        lock(&self.seen).clone()
    }

    pub(super) fn paths(&self) -> Vec<String> {
        self.seen()
            .iter()
            .map(|s| format!("{} {}", s.method, s.path))
            .collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop, which is waiting for a connection.
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(mut stream: TcpStream, seen: &Mutex<Vec<Seen>>, handler: &Mutex<Handler>) {
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    lock(seen).push(request.clone());
    let reply = {
        let mut handler = lock(handler);
        (*handler)(&request)
    };
    thread::sleep(reply.delay);
    let raw = format!(
        "HTTP/1.1 {} Status\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{}",
        reply.status,
        reply.body.len(),
        reply.body
    );
    let _ = stream.write_all(raw.as_bytes());
}

/// The request on `stream`, or `None` if the peer sent nothing (the wake-up connection).
fn read_request(stream: &mut TcpStream) -> Option<Seen> {
    let mut got = Vec::new();
    let mut buf = [0u8; 512];
    let end = loop {
        if let Some(end) = got.windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    };
    let head = String::from_utf8_lossy(&got[..end]).into_owned();
    let mut lines = head.lines();
    let mut request_line = lines.next()?.split(' ');
    let (method, path) = (request_line.next()?, request_line.next()?);
    let headers: BTreeMap<String, String> = lines
        .filter_map(|line| line.split_once(": "))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.to_owned()))
        .collect();
    let want: usize = headers
        .get("content-length")
        .map_or(0, |n| n.parse().unwrap());
    while got.len() < end + 4 + want {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
    Some(Seen {
        method: method.to_owned(),
        path: path.to_owned(),
        headers,
        body: got[end + 4..end + 4 + want].to_vec(),
    })
}
