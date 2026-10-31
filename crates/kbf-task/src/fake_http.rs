//! A small HTTP/1.1 server for tests: one handler answers every GET, each request is kept, and
//! connections are kept alive. It ends with the test process; nothing waits on it.

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// A request the server saw.
#[derive(Clone, Debug)]
pub(crate) struct Seen {
    /// The method.
    pub method: String,
    /// The path, with its query.
    pub path: String,
    /// The headers, names in lowercase.
    pub headers: Vec<(String, String)>,
}

impl Seen {
    /// The value of the header `name` (lowercase), if the request had it.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// What the handler answers.
pub(crate) struct Answer {
    /// The status code.
    pub status: u16,
    /// The body.
    pub body: Vec<u8>,
    /// Whether the connection ends before the body does.
    pub cut: bool,
    /// A `Location` header, for a redirect.
    pub location: Option<String>,
}

impl Answer {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            body: body.into(),
            cut: false,
            location: None,
        }
    }

    /// A redirect to `url`.
    pub fn redirect(status: u16, url: &str) -> Self {
        Self {
            location: Some(url.to_owned()),
            ..Self::new(status, "")
        }
    }

    /// An answer whose connection ends eight bytes before its body does.
    pub fn cut(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            cut: true,
            ..Self::new(status, body)
        }
    }
}

type Handler = dyn Fn(&Seen) -> Answer + Send + Sync;

/// A running fake server.
pub(crate) struct Fake {
    /// `http://127.0.0.1:<port>`.
    pub base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    /// Starts a server on a free loopback port.
    pub fn start(handler: impl Fn(&Seen) -> Answer + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<Handler> = Arc::new(handler);
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let stream = stream.expect("accept");
                let (handler, log) = (Arc::clone(&handler), Arc::clone(&log));
                std::thread::spawn(move || serve(stream, &*handler, &log));
            }
        });
        Self { base, seen }
    }

    /// Every request seen so far, in arrival order.
    pub fn requests(&self) -> Vec<Seen> {
        self.seen.lock().expect("seen").clone()
    }

    /// The paths of the requests seen so far.
    pub fn paths(&self) -> Vec<String> {
        self.requests().into_iter().map(|r| r.path).collect()
    }
}

fn serve(mut stream: std::net::TcpStream, handler: &Handler, log: &Mutex<Vec<Seen>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut line = String::new();
    while reader.read_line(&mut line).expect("request line") > 0 {
        let mut parts = line.split_whitespace();
        let method = parts.next().expect("method").to_owned();
        let path = parts.next().expect("path").to_owned();
        let mut headers = Vec::new();
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).expect("header");
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            let (name, value) = header.split_once(':').expect("header form");
            headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
        }
        let seen = Seen {
            method,
            path,
            headers,
        };
        log.lock().expect("seen").push(seen.clone());
        let answer = handler(&seen);
        let location = answer
            .location
            .as_ref()
            .map(|url| format!("Location: {url}\r\n"))
            .unwrap_or_default();
        let head = format!(
            "HTTP/1.1 {} X\r\n{location}Content-Length: {}\r\n\r\n",
            answer.status,
            answer.body.len() + if answer.cut { 8 } else { 0 }
        );
        stream.write_all(head.as_bytes()).expect("head");
        stream.write_all(&answer.body).expect("body");
        if answer.cut {
            return;
        }
        line.clear();
    }
}
