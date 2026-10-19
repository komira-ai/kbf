//! The deploy against a farm over TLS: the farm of the parent module served on a loopback
//! port, HTTP/1.1 under a certificate from a test CA, as the frontend's public port serves
//! it. The client is the real [`Https`]; the fake clock still moves only when the deploy
//! sleeps, so a run takes no real time but the handshakes.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

use super::super::api::{Https, Request, Trust};
use super::super::smoke_tests::pki;
use super::*;

/// The name the test certificate carries.
pub(super) const NAME: &str = "farm.example";
/// The operator token the client sends.
pub(super) const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef";

/// What the server saw, and how it answers.
pub(super) struct Served {
    /// Where it listens.
    pub(super) addr: SocketAddr,
    /// The PEM of the CA that vouches for its certificate.
    pub(super) ca: String,
    stop: AtomicBool,
    /// Close the TCP connection without the TLS goodbye.
    pub(super) abrupt: AtomicBool,
    /// The `Authorization` header of each request.
    pub(super) auth: Mutex<Vec<String>>,
    /// The `Content-Type` header of each request.
    pub(super) kinds: Mutex<Vec<String>>,
    /// Answer in chunks instead of with a `Content-Length`.
    pub(super) chunked: AtomicBool,
    /// Close the connection before the last byte of the body.
    pub(super) cut: AtomicBool,
    /// Requests that arrived over a TLS session that negotiated this ALPN protocol.
    pub(super) alpn: Mutex<Vec<Option<Vec<u8>>>>,
}

fn server_config(cert_pem: &str, key_pem: &str) -> Arc<ServerConfig> {
    let certs = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Arc::new(config)
}

/// Wakes the accept loop and stops it when the test ends, however it ends.
struct Stop<'a>(&'a AtomicBool, SocketAddr);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        self.0.store(true, SeqCst);
        let _ = TcpStream::connect(self.1);
    }
}

/// Serve `farm` on a loopback port and call `test` with the client for it and what the
/// server saw. The certificate is for [`NAME`] and the client trusts its CA only (and the
/// system's roots).
pub(super) fn with_tls<R>(farm: &Farm, test: impl FnOnce(&Https, &Served) -> R) -> R {
    let (ca, cert, key) = pki(NAME);
    let config = server_config(&cert, &key);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let served = Served {
        addr,
        ca: ca.clone(),
        stop: AtomicBool::new(false),
        abrupt: AtomicBool::new(false),
        auth: Mutex::default(),
        kinds: Mutex::default(),
        chunked: AtomicBool::new(false),
        cut: AtomicBool::new(false),
        alpn: Mutex::default(),
    };
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for conn in listener.incoming() {
                if served.stop.load(SeqCst) {
                    break;
                }
                // A client that gives up in the handshake is the client's test.
                let _ = conn.and_then(|tcp| serve_one(tcp, &config, farm, &served));
            }
        });
        let _stop = Stop(&served.stop, addr);
        let trust = Trust {
            server_name: Some(NAME.to_owned()),
            ca_pem: Some(ca.into_bytes()),
        };
        let https = Https::new(addr, &trust, TOKEN.to_owned(), Duration::from_secs(10)).unwrap();
        test(&https, &served)
    })
}

/// One request, one answer, then the connection closes.
fn serve_one(
    tcp: TcpStream,
    config: &Arc<ServerConfig>,
    farm: &Farm,
    served: &Served,
) -> io::Result<()> {
    tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
    let conn = ServerConnection::new(Arc::clone(config)).map_err(io::Error::other)?;
    let mut tls = StreamOwned::new(conn, tcp);
    let mut reader = BufReader::new(&mut tls);
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        if reader.read_until(b'\n', &mut head)? == 0 {
            return Ok(());
        }
    }
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut parsed = httparse::Request::new(&mut headers);
    parsed.parse(&head).map_err(io::Error::other)?;
    let header = |name: &str| {
        parsed
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| String::from_utf8_lossy(h.value).into_owned())
    };
    let length: usize = header("content-length").map_or(0, |v| v.trim().parse().unwrap());
    served
        .auth
        .lock()
        .unwrap()
        .push(header("authorization").unwrap_or_default());
    served
        .kinds
        .lock()
        .unwrap()
        .push(header("content-type").unwrap_or_default());
    let method = match parsed.method.unwrap_or_default() {
        "GET" => "GET",
        "POST" => "POST",
        _ => "PUT",
    };
    let path = parsed.path.unwrap_or_default().to_owned();
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    drop(reader);
    served
        .alpn
        .lock()
        .unwrap()
        .push(tls.conn.alpn_protocol().map(<[u8]>::to_vec));
    let request = Request {
        method,
        path,
        body,
        raw: false,
    };
    // A farm that cannot answer closes the connection, as a cut one does.
    let Ok(reply) = farm.answer(&request) else {
        return Ok(());
    };
    let mut out = format!("HTTP/1.1 {} Reply\r\nConnection: close\r\n", reply.status).into_bytes();
    if served.chunked.load(SeqCst) {
        let (first, second) = reply.body.split_at(reply.body.len() / 2);
        out.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
        for part in [first, second] {
            out.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
            out.extend_from_slice(part);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"0\r\n\r\n");
    } else {
        out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", reply.body.len()).as_bytes());
        out.extend_from_slice(&reply.body);
    }
    if served.cut.load(SeqCst) {
        out.truncate(out.len() - 1);
    }
    tls.write_all(&out)?;
    if !served.abrupt.load(SeqCst) {
        tls.conn.send_close_notify();
    }
    tls.flush()
}

/// [`run_with`] over TLS.
pub(super) fn run_tls(
    farm: &Farm,
    o: &Options,
    smoke: SmokeResult,
    bundle: Option<&mut dyn io::Read>,
) -> Summary {
    with_tls(farm, |https, _| {
        let clock = Clk(farm.now.clone());
        let smoke = FixedSmoke(smoke);
        let check = FixedCheck(Ok(true));
        let env = Env {
            api: https,
            clock: &clock,
            smoke: &smoke,
            check: &check,
        };
        deploy(&env, o, bundle)
    })
}
