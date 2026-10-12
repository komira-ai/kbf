//! Test helpers for this crate's own tests, beyond `kbf_host::testkit`: [`tls_server`],
//! an HTTPS server on a loopback port for the [`crate::https`] client and the `mdm`
//! kit; [`http_server`], a plain-HTTP one standing in for the MDM services' APIs on
//! loopback; and [`http`], the bytes of an answer either sends.

use std::io;

/// An HTTPS server on a loopback port, with a certificate for its names under a fresh
/// CA; each connection gets one answer.
pub struct TlsServer {
    pub addr: std::net::SocketAddr,
    /// The CA's certificate, for a client's roots.
    pub ca: rustls::pki_types::CertificateDer<'static>,
    /// The request paths served, in order.
    pub paths: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// The request lines and headers served, in order.
    pub heads: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl TlsServer {
    /// A client trusting only this server's CA.
    pub fn client(&self) -> crate::https::Client {
        let roots = crate::https::roots(vec![self.ca.clone()]).expect("the CA parses");
        crate::https::Client::with_roots(roots, std::time::Duration::from_secs(5))
    }

    /// `https://<name>:<port><path>`.
    pub fn url(&self, name: &str, path: &str) -> crate::https::HttpsUrl {
        let url = format!("https://{name}:{}{path}", self.addr.port());
        crate::https::HttpsUrl::new(&url).expect("a test URL")
    }
}

/// Serve on `127.0.0.1:0` as `names` (DNS names or IP addresses), answering each
/// request with the raw bytes `answer` makes of its path, then a TLS close.
pub fn tls_server(names: &[&str], answer: impl Fn(&str) -> Vec<u8> + Send + 'static) -> TlsServer {
    use std::sync::{Arc, Mutex};

    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = rcgen::CertifiedIssuer::self_signed(ca, rcgen::KeyPair::generate().expect("key"))
        .expect("CA");
    let key = rcgen::KeyPair::generate().expect("key");
    let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
    let cert = rcgen::CertificateParams::new(names)
        .expect("server params")
        .signed_by(&key, &ca)
        .expect("server certificate");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).expect("key der"),
        )
        .expect("server config");
    let config = Arc::new(config);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let paths = Arc::new(Mutex::new(Vec::new()));
    let heads = Arc::new(Mutex::new(Vec::new()));
    let seen = (Arc::clone(&paths), Arc::clone(&heads));
    std::thread::spawn(move || {
        for tcp in listener.incoming().flatten() {
            let _ = serve_one(&config, tcp, &answer, &seen);
        }
    });
    TlsServer {
        addr,
        ca: ca.der().clone(),
        paths,
        heads,
    }
}

type Seen = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

fn serve_one(
    config: &std::sync::Arc<rustls::ServerConfig>,
    tcp: std::net::TcpStream,
    answer: &dyn Fn(&str) -> Vec<u8>,
    seen: &(Seen, Seen),
) -> io::Result<()> {
    use std::io::{Read, Write};

    let conn =
        rustls::ServerConnection::new(std::sync::Arc::clone(config)).map_err(io::Error::other)?;
    let mut tls = rustls::StreamOwned::new(conn, tcp);
    let mut head = Vec::new();
    let mut byte = [0u8];
    while !head.ends_with(b"\r\n\r\n") {
        tls.read_exact(&mut byte)?;
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let path = text.split(' ').nth(1).unwrap_or_default().to_owned();
    seen.0.lock().expect("lock").push(path.clone());
    seen.1.lock().expect("lock").push(text);
    tls.write_all(&answer(&path))?;
    tls.conn.send_close_notify();
    tls.flush()
}

/// An HTTP/1.1 answer with `status` (`200 OK`) and `body`, by `Content-Length`.
pub fn http(status: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// One request a [`HttpServer`] was sent.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// `(lower-case name, value)`, in order.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A plain-HTTP server on a loopback port, as the MDM services' APIs are; each
/// connection gets one answer.
pub struct HttpServer {
    pub addr: std::net::SocketAddr,
    seen: std::sync::Arc<std::sync::Mutex<Vec<Request>>>,
}

impl HttpServer {
    /// The requests served, in order.
    pub fn requests(&self) -> Vec<Request> {
        self.seen.lock().expect("lock").clone()
    }
}

/// Serve on `127.0.0.1:0`, answering each request with the raw bytes `answer` makes of
/// it. A request is recorded before it is answered, so a test that saw its answer sees
/// the request too.
pub fn http_server(answer: impl Fn(&Request) -> Vec<u8> + Send + 'static) -> HttpServer {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::{Arc, Mutex};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for mut tcp in listener.incoming().flatten() {
            let mut r = BufReader::new(tcp.try_clone().expect("clone"));
            let mut first = String::new();
            r.read_line(&mut first).expect("request line");
            let mut words = first.split(' ');
            let method = words.next().unwrap_or_default().to_owned();
            let path = words.next().unwrap_or_default().to_owned();
            let mut headers = Vec::new();
            loop {
                let mut line = String::new();
                r.read_line(&mut line).expect("header");
                let line = line.trim_end();
                let Some((name, value)) = line.split_once(':') else {
                    break;
                };
                headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
            }
            let mut req = Request {
                method,
                path,
                headers,
                body: Vec::new(),
            };
            let length: usize = req
                .header("content-length")
                .map_or(0, |v| v.parse().expect("a length"));
            req.body = vec![0; length];
            r.read_exact(&mut req.body).expect("body");
            log.lock().expect("lock").push(req.clone());
            tcp.write_all(&answer(&req)).expect("answer");
        }
    });
    HttpServer { addr, seen }
}
